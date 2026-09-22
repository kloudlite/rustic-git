# sys-1 sessions Implementation Plan (backend)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the pi subprocess session manager in `harness/bench` with the sys-1 engine running in-process, a three-tier session tree persisted as append-only logs on `/bench`, and a sub lifecycle that clones, pushes into main and deletes.

**Architecture:** `harness/bench/src/engine/` is a source copy of `~/dev/jevharn/src` whose `tools.ts` talks HTTP to the workspace pod's tool server instead of the local filesystem. A `Session` actor per row appends rows to `sessions/{seq}.jsonl` and runs one turn at a time; a `Scheduler` starts turns for sessions with unread rows, repairs logs on boot and caps concurrency. `platform.ts` is the one `/v1` client (clone, delete, tool address); `sub.ts` is the spawn/push/deliver/delete state machine driven by log rows.

**Tech Stack:** Node 22 type-stripping (`node src/main.ts`, `node --test 'test/*.test.ts'`), no bun, `ws` at runtime; engine deps `ai`, `@ai-sdk/openai-compatible`, `@ai-sdk/anthropic`, `@ai-sdk/deepseek`, `@sinclair/typebox`. Rust: `crates/api` (authorized_keys), workspace prelude (`crates/workspaces/src/k8s/`), one SLO probe stage.

**Spec:** `docs/superpowers/specs/2026-09-22-sys1-sessions-design.md`

## Global Constraints

- Backend only. Nothing under `harness/src` (electron) or `web/` changes.
- Common term is "sys-1 engine" / "system one model". The string "jev engine" appears nowhere.
- Engine is copied source under `harness/bench/src/engine/`, never a package dependency. `@mariozechner/*` leaves `harness/bench` entirely (the harness root `package.json` keeps them for the electron app; not this plan's concern).
- Every engine runs in the bench process. No `child_process` import anywhere under `harness/bench/src/` after Task 9 (`rpc-child.ts` is deleted). Workspace pods are reached only over `http://{podIP}:7788/tools/{name}`.
- Tiers and allow-lists (spec §2): top = `delegate ask_user tell_user think recall`; main = top's plus `read glob grep bash_output` plus `push`; sub = every tool. `readOnly: true` for main.
- Every state change is a log row written before it is acknowledged; two-write orders exactly as spec §1 (child `user` row then parent `delegate`; child `turn.end` then parent `user`).
- `MAX_RUNNING = 8`. Tests are `node --test`, no cluster, no network beyond `127.0.0.1`.
- Commit subjects imperative sentence case, no tool attribution. Comments say why. Files under ~800 lines.
- **Spec deviation, flagged to the owner in the handoff:** ruling 8 names an "operation executor" as the mutation layer. No such code exists in this repo (only `docs/superpowers/specs/2026-09-18-harness-operation-executor-*.md`). This plan puts every platform mutation behind one thin client, `platform.ts` (Task 4), so a future executor replaces one file. Approvals are not implemented here.
- Rust tasks (11, 12) run in the dev pod per `deploy/dev/README.md`; never `cargo` on the laptop.

---

## File map

Create:
- `harness/bench/src/rows.ts` — log row types and derived reads (unread, open turn, repair candidates).
- `harness/bench/src/platform.ts` — `/v1` client: config file, `tools(ws)`, `clone(ws, name)`, `remove(ws)`.
- `harness/bench/src/engine/**` — copied from `~/dev/jevharn/src` minus `cli.ts ui.ts approve.ts`.
- `harness/bench/src/engine/remote.ts` — per-cwd tool backend registry and the HTTP client.
- `harness/bench/src/session.ts` — `Session` actor: rows, `turn()`, tier tools.
- `harness/bench/src/scheduler.ts` — boot repair, run loop, cap.
- `harness/bench/src/sub.ts` — sub lifecycle (spawn, push, deliver, delete).
- `harness/bench/src/runtime.ts` — production engine wiring (`makeAiSdkLlm`, `ask`, `runTask`).
- `harness/bench/test/fake-tools.ts` — in-process fake tool server.
- `harness/bench/test/{rows,platform,remote,session,scheduler,sub,tree}.test.ts`.

Modify:
- `harness/bench/package.json` — engine deps.
- `harness/bench/src/sessions.ts` — `parent`, `tier`, `state`, `bySeq`, `children`, `logFile`.
- `harness/bench/src/bench.ts` — swap `RpcChild` for `Session`/`Scheduler`.
- `harness/bench/src/server.ts` — `GET /sessions/{id}/children`, rows on `/events`.
- `crates/api/src/credentials.rs` — `authorized_keys_for` appends the platform public key.
- workspace prelude — `git config --global receive.denyCurrentBranch updateInstead`.
- `bins/slo` + `crates/workspaces/src/slo/catalogue.rs` + `deploy/slo.md` — `bench.delegate` probe.

Delete: `harness/bench/src/rpc-child.ts`, `harness/bench/test/rpc-child.test.ts`, `harness/bench/test/fake-pi.ts`.

---

### Task 1: Copy the engine in and make it load under node

**Files:**
- Create: `harness/bench/src/engine/*` (copy)
- Modify: `harness/bench/package.json`
- Test: `harness/bench/test/engine-loads.test.ts`

**Interfaces:**
- Produces: `harness/bench/src/engine/index.ts` re-exporting `runTask`, `RunTaskDeps`, `Llm`, `LlmSession`, `LlmTool`, `Ask`, `ask`, `makeAiSdkLlm`, `TOOLS`, `Tool`, `RunCtx`, `User`, `runTool`, `readNotes`, `addNotes`.

- [ ] **Step 1: Copy the source, drop the terminal files**

```bash
mkdir -p harness/bench/src/engine
cp ~/dev/jevharn/src/*.ts harness/bench/src/engine/
rm harness/bench/src/engine/{cli,ui,approve}.ts
grep -ln "pi-coding-agent\|pi-tui" harness/bench/src/engine/*.ts   # expect: nothing
```

If `grep` lists a file, delete only the import and the lines that use it; note it in the commit body.

- [ ] **Step 2: Add the engine's runtime deps to the bench package**

`harness/bench/package.json` `dependencies` (add the key if absent):

```json
"dependencies": {
  "@ai-sdk/anthropic": "^4.0.58",
  "@ai-sdk/deepseek": "^3.0.50",
  "@ai-sdk/openai-compatible": "^3.0.53",
  "@sinclair/typebox": "^0.34.52",
  "ai": "^7.0.108",
  "ws": "^8.21.3"
}
```

Run `cd harness/bench && npm install`.

- [ ] **Step 3: Fix `index.ts` to export only what remains**

Open `harness/bench/src/engine/index.ts`; remove every export that named `cli`, `ui` or `approve`. Ensure it exports the names in **Produces** (add `export { runTool, TOOLS, readNotes, addNotes } from ...` lines if missing).

- [ ] **Step 4: Write the failing load test**

`harness/bench/test/engine-loads.test.ts`:

```ts
import { test } from "node:test";
import assert from "node:assert/strict";

test("the engine loads without pi and exposes the seam", async () => {
  const e = await import("../src/engine/index.ts");
  for (const k of ["runTask", "ask", "makeAiSdkLlm", "TOOLS", "runTool", "readNotes", "addNotes"]) assert.ok(k in e, k);
  assert.ok(e.TOOLS.some((t: { name: string }) => t.name === "bash"));
});
```

- [ ] **Step 5: Run it**

Run: `cd harness/bench && node --test test/engine-loads.test.ts`
Expected: PASS. If node refuses a syntax that tsx accepted (enums, parameter properties, `import x = require`), rewrite that one site to plain TS and note it in the commit body.

- [ ] **Step 6: Commit**

```bash
git add harness/bench/package.json harness/bench/package-lock.json harness/bench/src/engine harness/bench/test/engine-loads.test.ts
git commit -m "Copy the sys-1 engine into the bench as source"
```

---

### Task 2: Log rows

**Files:**
- Create: `harness/bench/src/rows.ts`
- Test: `harness/bench/test/rows.test.ts`

**Interfaces:**
- Consumes: `appendLine`, `readLines` from `src/log.ts`.
- Produces:

```ts
export type Row =
  | { kind: "user"; ts: number; from: "person" | number; text: string; childTurn?: number }
  | { kind: "turn.start"; ts: number; turn: number }
  | { kind: "turn.step"; ts: number; turn: number; step: string }
  | { kind: "turn.end"; ts: number; turn: number; answer?: string; error?: string; commit?: string }
  | { kind: "delegate"; ts: number; turn: number; child: number; instruction: string }
  | { kind: "interrupted"; ts: number; turn: number };
export function readRows(file: string): Row[];
export function append(file: string, row: Row): void;
export function unread(rows: Row[]): Extract<Row, { kind: "user" }>[];
export function openTurn(rows: Row[]): number | undefined;   // turn.start with no turn.end/interrupted
export function nextTurn(rows: Row[]): number;               // 1 + highest turn seen
export function lastEnd(rows: Row[]): Extract<Row, { kind: "turn.end" }> | undefined;
```

- [ ] **Step 1: Write the failing tests**

`harness/bench/test/rows.test.ts`:

```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { append, readRows, unread, openTurn, nextTurn, lastEnd, type Row } from "../src/rows.ts";

const tmp = () => path.join(fs.mkdtempSync(path.join(os.tmpdir(), "rows-")), "1.jsonl");
const u = (text: string, ts = 1): Row => ({ kind: "user", ts, from: "person", text });

test("unread is every user row after the last turn.end", () => {
  const f = tmp();
  append(f, u("a"));
  append(f, { kind: "turn.start", ts: 2, turn: 1 });
  append(f, { kind: "turn.end", ts: 3, turn: 1, answer: "ok" });
  append(f, u("b", 4));
  append(f, u("c", 5));
  const rows = readRows(f);
  assert.deepEqual(unread(rows).map((r) => r.text), ["b", "c"]);
  assert.equal(openTurn(rows), undefined);
  assert.equal(nextTurn(rows), 2);
  assert.equal(lastEnd(rows)?.answer, "ok");
});

test("a turn.start without an end is open; an interrupted one is not", () => {
  const f = tmp();
  append(f, u("a"));
  append(f, { kind: "turn.start", ts: 2, turn: 1 });
  assert.equal(openTurn(readRows(f)), 1);
  append(f, { kind: "interrupted", ts: 3, turn: 1 });
  assert.equal(openTurn(readRows(f)), undefined);
  // the user row stays unread: an interrupted turn answers nothing
  assert.equal(unread(readRows(f)).length, 1);
});

test("a missing file reads as no rows", () => {
  assert.deepEqual(readRows(tmp()), []);
  assert.equal(nextTurn([]), 1);
});
```

- [ ] **Step 2: Run to see it fail**

Run: `cd harness/bench && node --test test/rows.test.ts`
Expected: FAIL, cannot find `../src/rows.ts`.

- [ ] **Step 3: Implement**

`harness/bench/src/rows.ts`:

```ts
// One append-only jsonl per session is the whole conversation (spec §1). Everything the scheduler
// needs — what is unread, whether a turn is open — is derived from the rows on every read, never
// stored beside them, so a crash between two writes can only lose work, never invent state.
import fs from "node:fs";
import { appendLine, readLines } from "./log.ts";

export type Row =
  | { kind: "user"; ts: number; from: "person" | number; text: string; childTurn?: number }
  | { kind: "turn.start"; ts: number; turn: number }
  | { kind: "turn.step"; ts: number; turn: number; step: string }
  | { kind: "turn.end"; ts: number; turn: number; answer?: string; error?: string; commit?: string }
  | { kind: "delegate"; ts: number; turn: number; child: number; instruction: string }
  | { kind: "interrupted"; ts: number; turn: number };

export const readRows = (file: string): Row[] => (fs.existsSync(file) ? readLines<Row>(file) : []);
export const append = (file: string, row: Row) => appendLine(file, row);

export function unread(rows: Row[]) {
  let from = 0;
  rows.forEach((r, i) => { if (r.kind === "turn.end") from = i + 1; });
  return rows.slice(from).filter((r): r is Extract<Row, { kind: "user" }> => r.kind === "user");
}

export function openTurn(rows: Row[]): number | undefined {
  let open: number | undefined;
  for (const r of rows) {
    if (r.kind === "turn.start") open = r.turn;
    else if ((r.kind === "turn.end" || r.kind === "interrupted") && r.turn === open) open = undefined;
  }
  return open;
}

export const nextTurn = (rows: Row[]) => 1 + rows.reduce((m, r) => ("turn" in r ? Math.max(m, r.turn) : m), 0);

export function lastEnd(rows: Row[]) {
  for (let i = rows.length - 1; i >= 0; i--) if (rows[i].kind === "turn.end") return rows[i] as Extract<Row, { kind: "turn.end" }>;
  return undefined;
}
```

Check `readLines` in `src/log.ts` tolerates a torn last line (it should skip an unparsable tail); if it throws, wrap the parse in that function to skip the last line only and add a test there.

- [ ] **Step 4: Run to see it pass**

Run: `cd harness/bench && node --test test/rows.test.ts`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add harness/bench/src/rows.ts harness/bench/test/rows.test.ts
git commit -m "Add the session log row model"
```

---

### Task 3: Session tree fields

**Files:**
- Modify: `harness/bench/src/sessions.ts`
- Test: `harness/bench/test/sessions.test.ts` (append)

**Interfaces:**
- Produces on `SessionRow`: `parent?: number; tier?: "top" | "main" | "sub"; state?: "open" | "closed"`. On `SessionList`: `bySeq(seq: number): SessionRow | undefined`, `children(seq: number): SessionRow[]` (open and closed, seq order), `logFile(row: SessionRow): string` = `{dir}/sessions/{seq}.jsonl`, and `create(model?, extra?: Partial<SessionRow>)`.

- [ ] **Step 1: Failing tests**

Append to `harness/bench/test/sessions.test.ts` (reuse its existing tmp-dir helper; if none, `fs.mkdtempSync`):

```ts
test("tree fields persist and children are found by parent seq", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "tree-"));
  const l = new SessionList(dir);
  const top = l.create(undefined, { tier: "top", state: "open" });
  const main = l.create(undefined, { tier: "main", state: "open", workspace: "ws1" });
  const sub = l.create(undefined, { tier: "sub", state: "open", parent: main.seq, workspace: "ws1-c1" });
  assert.deepEqual(l.children(main.seq).map((s) => s.seq), [sub.seq]);
  assert.equal(l.bySeq(top.seq)?.tier, "top");
  assert.equal(l.logFile(sub), path.join(dir, "sessions", `${sub.seq}.jsonl`));
  const again = new SessionList(dir);
  assert.equal(again.bySeq(sub.seq)?.parent, main.seq);
});
```

- [ ] **Step 2: Run, expect failure**

Run: `cd harness/bench && node --test test/sessions.test.ts`
Expected: FAIL, `create` ignores `extra` / `children` is not a function.

- [ ] **Step 3: Implement**

In `harness/bench/src/sessions.ts`: add the three optional fields to `SessionRow`; change `create(model?: string)` to `create(model?: string, extra: Partial<SessionRow> = {})` and spread `...extra` into the new row before persisting; add

```ts
  bySeq(seq: number) { return this.all().find((s) => s.seq === seq); }
  children(seq: number) { return this.all().filter((s) => s.parent === seq).sort((a, b) => a.seq - b.seq); }
  logFile(row: SessionRow) { return path.join(this.dir, "sessions", `${row.seq}.jsonl`); }
```

(`this.dir` — use whatever the class already stores the folder as; `mkdirSync(join(dir, "sessions"), { recursive: true })` in the constructor.)

- [ ] **Step 4: Run, expect pass**

Run: `cd harness/bench && node --test test/sessions.test.ts`

- [ ] **Step 5: Commit**

```bash
git add harness/bench/src/sessions.ts harness/bench/test/sessions.test.ts
git commit -m "Give session rows a parent, a tier and a state"
```

---

### Task 4: Platform client

**Files:**
- Create: `harness/bench/src/platform.ts`
- Test: `harness/bench/test/platform.test.ts`

**Interfaces:**
- Produces:

```ts
export class Platform {
  constructor(api: string, token: string, team?: string);
  static fromEnv(): Platform;        // $KL_CONFIG_DIR/config.json or ~/.config/kl-connect/config.json, {api, token}; team from $KL_TEAM
  tools(ws: string): Promise<string>;               // "10.0.0.5:7788" from GET /v1/workspaces/{ws}/tools -> {address}
  clone(ws: string, name: string): Promise<string>; // new workspace id from POST /v1/workspaces/{ws}/clone {name}
  remove(ws: string): Promise<void>;                // DELETE /v1/workspaces/{ws}
}
export class PlatformError extends Error { status: number; body: string }
```

Copy `call()` from `harness/pi/kloudlite.ts` (Bearer header, `?team=` query when set, HTML-page detection) — this is the one place the operation executor would later replace.

- [ ] **Step 1: Failing test with a local fake `/v1`**

`harness/bench/test/platform.test.ts`:

```ts
import { test, after } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { Platform, PlatformError } from "../src/platform.ts";

const hits: { method?: string; url?: string; auth?: string; body: string }[] = [];
const srv = http.createServer((req, res) => {
  let body = "";
  req.on("data", (c) => (body += c));
  req.on("end", () => {
    hits.push({ method: req.method, url: req.url, auth: req.headers.authorization, body });
    const [, , , ws, verb] = req.url!.split("?")[0].split("/");
    if (verb === "tools") return res.end(JSON.stringify({ address: "10.0.0.5:7788" }));
    if (verb === "clone") { res.statusCode = 202; return res.end(JSON.stringify({ id: `${ws}-c1` })); }
    if (req.method === "DELETE") { res.statusCode = 204; return res.end(); }
    res.statusCode = 409; res.end(JSON.stringify({ error: "workspaces: 3 of 3 in use" }));
  });
});
await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
after(() => srv.close());
const api = `http://127.0.0.1:${(srv.address() as { port: number }).port}`;

test("tools, clone and remove hit the right routes with the token", async () => {
  const p = new Platform(api, "tok", "team1");
  assert.equal(await p.tools("ws1"), "10.0.0.5:7788");
  assert.equal(await p.clone("ws1", "sub-1"), "ws1-c1");
  await p.remove("ws1-c1");
  assert.deepEqual(hits.map((h) => `${h.method} ${h.url}`), [
    "GET /v1/workspaces/ws1/tools?team=team1",
    "POST /v1/workspaces/ws1/clone?team=team1",
    "DELETE /v1/workspaces/ws1-c1?team=team1",
  ]);
  assert.ok(hits.every((h) => h.auth === "Bearer tok"));
  assert.equal(JSON.parse(hits[1].body).name, "sub-1");
});

test("a refused call is a PlatformError carrying status and body", async () => {
  const p = new Platform(api, "tok");
  await assert.rejects(p.clone("ws1", "x"), (e: PlatformError) => e instanceof PlatformError && e.status === 409 && /3 of 3/.test(e.body));
});
```

(The fake answers 409 for anything unrouted; the second test's clone must reach that branch — use a workspace id the fake does not special-case, e.g. `"nope"` with URL `/v1/workspaces/nope/clone`: adjust the fake so only `ws1` clones succeed.)

- [ ] **Step 2: Run, expect failure**

Run: `cd harness/bench && node --test test/platform.test.ts`

- [ ] **Step 3: Implement**

`harness/bench/src/platform.ts`:

```ts
// The bench's only /v1 client. Ruling 8 names an operation executor as the mutation layer; none
// exists in the tree yet, so every clone and delete funnels through here and a later executor
// replaces this file, not its callers.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

export class PlatformError extends Error {
  constructor(public status: number, public body: string) { super(`platform ${status}: ${body.slice(0, 200)}`); }
}

export class Platform {
  constructor(private api: string, private token: string, private team?: string) {}

  static fromEnv(): Platform {
    const dir = process.env.KL_CONFIG_DIR ?? path.join(os.homedir(), ".config", "kl-connect");
    const c = JSON.parse(fs.readFileSync(path.join(dir, "config.json"), "utf8")) as { api?: string; token: string };
    return new Platform(c.api ?? "https://dev.kloudlite.io", c.token, process.env.KL_TEAM);
  }

  private async call(method: string, p: string, body?: unknown): Promise<unknown> {
    const url = `${this.api}${p}${this.team ? `?team=${encodeURIComponent(this.team)}` : ""}`;
    const r = await fetch(url, { method, headers: { authorization: `Bearer ${this.token}`, "content-type": "application/json" }, body: body === undefined ? undefined : JSON.stringify(body) });
    const text = await r.text();
    if (!r.ok) throw new PlatformError(r.status, text);
    if (text.trimStart().startsWith("<")) throw new PlatformError(r.status, "html page, not the api"); // a login redirect answers 200 with HTML
    return text ? JSON.parse(text) : undefined;
  }

  async tools(ws: string) { return ((await this.call("GET", `/v1/workspaces/${encodeURIComponent(ws)}/tools`)) as { address: string }).address; }
  async clone(ws: string, name: string) { return ((await this.call("POST", `/v1/workspaces/${encodeURIComponent(ws)}/clone`, { name })) as { id: string }).id; }
  async remove(ws: string) { await this.call("DELETE", `/v1/workspaces/${encodeURIComponent(ws)}`); }
}
```

Check the real response shapes against `harness/pi/workspace-tools.ts:121` (tools) and `harness/pi/kloudlite.ts:118` (clone) — if the clone answer nests the id (e.g. `{workspace: {id}}`), read that field and fix the fake.

- [ ] **Step 4: Run, expect pass**

- [ ] **Step 5: Commit**

```bash
git add harness/bench/src/platform.ts harness/bench/test/platform.test.ts
git commit -m "Add the bench platform client"
```

---

### Task 5: Tools over HTTP

**Files:**
- Create: `harness/bench/src/engine/remote.ts`, `harness/bench/test/fake-tools.ts`
- Modify: `harness/bench/src/engine/tools.ts`, `harness/bench/src/engine/executor.ts:333-340` (notes), `harness/bench/src/engine/router.ts:64` (isDir)
- Test: `harness/bench/test/remote.test.ts`

**Interfaces:**
- Produces:

```ts
// remote.ts
export type Backend = (name: string, args: Record<string, unknown>) => Promise<unknown>;
export function setBackend(cwd: string, b: Backend | undefined): void;
export function httpBackend(address: string): Backend;   // POST http://{address}/tools/{name}
export function remote(cwd: string, name: string, args: Record<string, unknown>): Promise<unknown>; // throws "no tool backend for {cwd}"
export function text(v: unknown): string;               // stdout+stderr / content / JSON fallback
```

The engine's `cwd` string becomes a KEY (the workspace id), never a path on the bench. Every `run(cwd, …)` in `tools.ts` resolves through `remote(cwd, …)`.

- [ ] **Step 1: Fake tool server**

`harness/bench/test/fake-tools.ts`:

```ts
// One in-process handler standing in for `kl ide serve`: a map of files, a scripted exec.
import http from "node:http";

export type Fake = { address: string; files: Map<string, string>; execs: string[]; exec: (cmd: string) => { exit_code: number; stdout: string; stderr: string }; close: () => void };

export async function fakeTools(exec: Fake["exec"] = () => ({ exit_code: 0, stdout: "", stderr: "" })): Promise<Fake> {
  const files = new Map<string, string>();
  const execs: string[] = [];
  const srv = http.createServer((req, res) => {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      const name = req.url!.replace("/tools/", "");
      const a = body ? JSON.parse(body) : {};
      const ok = (v: unknown) => res.end(JSON.stringify(v));
      const err = (code: number, m: string) => { res.statusCode = code; res.end(JSON.stringify({ error: m })); };
      if (req.method === "GET" && req.url === "/tools") return ok({ tools: [...new Set(["read", "write", "edit", "glob", "grep", "exec", "process_output", "process_kill"])].map((n) => ({ name: n })) });
      switch (name) {
        case "read": { const p = a.paths?.[0] ?? a.path; return files.has(p) ? ok({ path: p, content: files.get(p), total_lines: files.get(p)!.split("\n").length }) : err(404, `no such file: ${p}`); }
        case "write": files.set(a.path, a.content); return ok({ path: a.path, bytes: a.content.length });
        case "edit": { const f = a.files[0]; const cur = files.get(f.path) ?? ""; if (!cur.includes(f.edits[0].old)) return err(400, "old not found"); files.set(f.path, cur.replace(f.edits[0].old, f.edits[0].new)); return ok({ path: f.path, applied: 1 }); }
        case "glob": return ok({ cwd: ".", paths: [...files.keys()], truncated: false });
        case "grep": return ok({ matches: [...files].flatMap(([p, c]) => c.split("\n").map((t, i) => ({ path: p, line: i + 1, text: t })).filter((m) => m.text.includes(a.pattern))) });
        case "exec": execs.push(a.cmd); return ok(fake.exec(a.cmd));
        case "process_output": return ok({ stdout: "", stderr: "", next: 0 });
        case "process_kill": return ok({ ok: true });
        default: return err(404, `no tool ${name}`);
      }
    });
  });
  await new Promise<void>((r) => srv.listen(0, "127.0.0.1", r));
  const fake: Fake = { address: `127.0.0.1:${(srv.address() as { port: number }).port}`, files, execs, exec, close: () => srv.close() };
  return fake;
}
```

- [ ] **Step 2: Failing tests**

`harness/bench/test/remote.test.ts`:

```ts
import { test, after } from "node:test";
import assert from "node:assert/strict";
import { fakeTools } from "./fake-tools.ts";
import { setBackend, httpBackend, remote } from "../src/engine/remote.ts";
import { TOOLS, runTool } from "../src/engine/index.ts";

const fake = await fakeTools((cmd) => ({ exit_code: cmd.includes("false") ? 1 : 0, stdout: `ran ${cmd}`, stderr: "" }));
after(() => fake.close());
setBackend("ws1", httpBackend(fake.address));
const tool = (n: string) => TOOLS.find((t) => t.name === n)!;

test("remote posts to /tools/{name} and a 4xx is an error string, not a throw", async () => {
  assert.deepEqual(await remote("ws1", "write", { path: "a.txt", content: "hi\n" }), { path: "a.txt", bytes: 3 });
  await assert.rejects(remote("nope", "read", {}), /no tool backend for nope/);
});

test("bash maps to exec and returns stdout then stderr", async () => {
  const out = await runTool("ws1", tool("bash"), { command: "echo x" });
  assert.match(out, /ran echo x/);
  assert.equal(fake.execs.at(-1), "echo x");
});

test("read, write, edit, glob, grep go to the pod by their own names", async () => {
  assert.match(await runTool("ws1", tool("write"), { path: "src/a.ts", content: "const a = 1;\n" }), /src\/a\.ts/);
  assert.match(await runTool("ws1", tool("read"), { path: "src/a.ts" }), /const a = 1/);
  assert.match(await runTool("ws1", tool("edit"), { path: "src/a.ts", old_string: "1", new_string: "2" }), /-.*1[\s\S]*\+.*2/);
  assert.equal(fake.files.get("src/a.ts"), "const a = 2;\n");
  assert.match(await runTool("ws1", tool("glob"), { folder: "all folders", pattern: "**" }), /src\/a\.ts/);
  assert.match(await runTool("ws1", tool("grep"), { pattern: "const", path: "all files" }), /src\/a\.ts:1:/);
});

test("a missing file is a tool result the model can read", async () => {
  const out = await runTool("ws1", tool("read"), { path: "none.txt" });
  assert.match(out, /no such file/);
});
```

- [ ] **Step 3: Run, expect failure**

Run: `cd harness/bench && node --test test/remote.test.ts`

- [ ] **Step 4: Implement `remote.ts`**

```ts
// The engine's `cwd` is a key, not a path: the bench holds no source (ruling 2). Each key maps to
// one workspace's tool server; path confinement is the pod's own `paths::confine`.
export type Backend = (name: string, args: Record<string, unknown>) => Promise<unknown>;
const backends = new Map<string, Backend>();
export const setBackend = (cwd: string, b: Backend | undefined) => { b ? backends.set(cwd, b) : backends.delete(cwd); };

export class ToolError extends Error { constructor(public status: number, message: string) { super(message); } }

export const httpBackend = (address: string): Backend => async (name, args) => {
  const r = await fetch(`http://${address}/tools/${name}`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(args), signal: AbortSignal.timeout(600_000) });
  const v = (await r.json().catch(() => ({ error: `bad json from ${name}` }))) as { error?: string };
  if (!r.ok) throw new ToolError(r.status, v.error ?? `${name}: ${r.status}`);
  return v;
};

export function remote(cwd: string, name: string, args: Record<string, unknown>) {
  const b = backends.get(cwd);
  if (!b) throw new Error(`no tool backend for ${cwd}`);
  return b(name, args);
}

// A tool result is always a string to the model; a failed call is a result too, never an exception (tools.ts's own rule).
export const text = (v: unknown): string => {
  if (typeof v === "string") return v;
  const o = v as Record<string, unknown>;
  if (o && typeof o.stdout === "string") return `${o.stdout}${o.stderr ? `\n${o.stderr}` : ""}${o.exit_code ? `\nexit ${o.exit_code}` : ""}`;
  if (o && typeof o.content === "string") return o.content;
  return JSON.stringify(v);
};
export const tryRemote = (cwd: string, name: string, args: Record<string, unknown>) => remote(cwd, name, args).catch((e) => `error: ${(e as Error).message}`);
```

- [ ] **Step 5: Rewrite the local IO sites in `tools.ts`**

Delete the `node:fs`, `node:fs/promises`, `node:child_process` imports and the `procs` map/`killAllProcs`/`procIds`/`procSummary` bodies that reach `child.pid` (keep the exports as thin wrappers over `remote(cwd, "process_list", {})` when a caller still needs them; grep `executor.ts` and `task.ts` for `procSummary`). Then, site by site (line numbers are jevharn's; re-grep after the imports go):

| jevharn site | replacement |
|---|---|
| `sh(cwd, file, args)` line 44 | `text(await tryRemote(cwd, "exec", { cmd: [file, ...args].map(q).join(" ") }))` where `q = (s) => "'" + s.replace(/'/g, "'\\''") + "'"` |
| `bash` run (`spawn("sh",["-c",command],{detached})` ~107) | foreground: `text(await tryRemote(cwd, "exec", { cmd: command, timeout_ms: BG_WAIT_MS }))`; background (`a.shell === "background"` or whatever the existing branch keys on): `const r = await tryRemote(cwd, "exec", { cmd: command, detach: true }); return typeof r === "string" ? r : `started ${(r as {id:string}).id}`` |
| `bash_output` (~478, reads `procs/{id}.log`) | `text(await tryRemote(cwd, "process_output", { id: a.proc, since: 0 }))` |
| `kill_shell` (~73 `process.kill`) | `text(await tryRemote(cwd, "process_kill", { id: a.proc }))` |
| `read` (~374, 380 `readFile(inside(cwd,p))`) | `const r = await tryRemote(cwd, "read", { path: a.path, offset, limit })` where `offset/limit` come from the existing `lines` parse (line 275); on a string (error) return it; else `fileView(a.path, (r as {content:string}).content, a.lines)` |
| `write` (~442) | `text(await tryRemote(cwd, "write", { path: a.path, content: a.content }))` keeping the existing "what was written" rendering |
| `edit` (~404-415 read+replace+write) | `tryRemote(cwd, "edit", { files: [{ path: a.path, edits: [{ old: a.old_string, new: a.new_string }] }] })`; keep the `-`/`+` rendering computed from the args |
| `glob`/`files(cwd)` (~163 `readdir`, 328) | `((await remote(cwd, "glob", { pattern: "**/*" })) as {paths:string[]}).paths` |
| `grep` (~220 `git grep`) | `remote(cwd, "grep", { pattern, glob: a.path === ALL ? undefined : a.path, mode: "content" })` rendered as `path:line:text` |
| manifests/`heads` (~97, 161, 206 `existsSync`, `readFile`) | one `remote(cwd,"read",{paths:[...]})` for the named files; a `files` entry with `error` is skipped |
| digests (~263 `stat`+`readFile`) | `read` with `paths`, skip entries whose `total_lines*80 > DIGEST_MAX_BYTES` (approximation; note with `// ponytail:`) |
| `recall` (~464-467 `.jevharn/sections.json`, `messages.jsonl`) | return `"error: recall is answered from the session log"` — Task 6 overrides `recall` with a bench tool |
| `inside(cwd, p)` (131) | delete; confinement is the pod's |

`executor.ts` `readNotes`/`addNotes` (lines 333-340): make both async over `remote(cwd, "read", {path: ".jevharn/project.md"})` / `remote(cwd, "write", …)`; update their two callers (`makePlanner`, and wherever `addNotes` is awaited — grep) to `await`. `router.ts:64` `isDir`: replace `statSync` with a synchronous `false` and a comment: directory detection needs the pod; the model's `cd` in the literal still works.

- [ ] **Step 6: Run, expect pass**

Run: `cd harness/bench && node --test test/remote.test.ts test/engine-loads.test.ts`
Then: `grep -rn "node:fs\|child_process" harness/bench/src/engine/` — expected: only `sections.ts` (unused by the bench after Task 6; delete it and its `Summarise` type import in `executor.ts` if nothing else references it).

- [ ] **Step 7: Commit**

```bash
git add harness/bench/src/engine harness/bench/test/fake-tools.ts harness/bench/test/remote.test.ts
git commit -m "Point the engine's tools at the workspace tool server"
```

---

### Task 6: Session actor and tier tools

**Files:**
- Create: `harness/bench/src/session.ts`
- Test: `harness/bench/test/session.test.ts`

**Interfaces:**
- Consumes: `rows.ts`, `sessions.ts` (Task 3), `engine/index.ts` `Tool`, `TOOLS`, `User`.
- Produces:

```ts
export type Turn = (ctx: TurnCtx) => Promise<string>;   // the engine seam; production = runtime.ts (Task 10), tests script it
export type TurnCtx = { prompt: string; cwd: string; tools: Tool[]; user: User; readOnly: boolean; log: (step: string) => void; signal: AbortSignal; history: Row[] };
export type Hooks = {
  delegate: (from: Session, target: string, instruction: string) => Promise<string>;  // Task 8 / bench wiring
  push?: (from: Session) => Promise<string>;
  tell: (from: Session, text: string) => void;      // to the person (top) — bench emits on /events
  askPerson: (from: Session, q: string) => Promise<string>;
};
export const TIER_TOOLS: Record<"top" | "main" | "sub", string[] | "all">;
export class Session {
  constructor(row: SessionRow, file: string, turnFn: Turn, hooks: Hooks);
  readonly row: SessionRow; readonly file: string;
  running: boolean;
  rows(): Row[];
  receive(from: "person" | number, text: string, childTurn?: number): void;   // appends a user row
  hasUnread(): boolean;
  turn(): Promise<void>;     // turn.start → engine → turn.end{answer|error}; then onAnswer
  abort(): void;             // interrupted row, running=false
  onAnswer?: (s: Session, end: Extract<Row, { kind: "turn.end" }>) => Promise<void>;   // scheduler installs: deliver to parent
}
```

- [ ] **Step 1: Failing tests**

`harness/bench/test/session.test.ts`:

```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Session, TIER_TOOLS, type Turn, type Hooks } from "../src/session.ts";
import { readRows } from "../src/rows.ts";
import type { SessionRow } from "../src/sessions.ts";

const row = (seq: number, tier: "top" | "main" | "sub"): SessionRow => ({ id: `s${seq}`, name: `s${seq}`, seq, created: 0, lastActive: 0, archived: false, tier, state: "open", workspace: tier === "top" ? undefined : `ws${seq}` });
const file = () => path.join(fs.mkdtempSync(path.join(os.tmpdir(), "sess-")), "1.jsonl");
const hooks = (over: Partial<Hooks> = {}): Hooks => ({ delegate: async () => "delegated", tell: () => {}, askPerson: async () => "yes", ...over });

test("a turn joins unread rows into one prompt and writes start then end", async () => {
  const seen: string[] = [];
  const turn: Turn = async (c) => { seen.push(c.prompt); return "answer"; };
  const s = new Session(row(1, "main"), file(), turn, hooks());
  s.receive("person", "one");
  s.receive(3, "two");
  await s.turn();
  assert.equal(seen.length, 1);
  assert.match(seen[0], /one[\s\S]*two/);
  assert.deepEqual(readRows(s.file).map((r) => r.kind), ["user", "user", "turn.start", "turn.end"]);
  assert.equal(s.hasUnread(), false);
  assert.equal(s.running, false);
});

test("an engine failure ends the turn with error and leaves the rows unread", async () => {
  const s = new Session(row(1, "main"), file(), async () => { throw new Error("llm down"); }, hooks());
  s.receive("person", "x");
  await s.turn();
  const end = readRows(s.file).at(-1) as { kind: string; error?: string };
  assert.equal(end.kind, "turn.end");
  assert.match(end.error!, /llm down/);
  assert.equal(s.hasUnread(), true);
});

test("tier allow-lists: top has no workspace tools, main is read-only, sub has all", () => {
  const names = (t: "top" | "main" | "sub") => new Session(row(1, t), file(), async () => "", hooks()).tools().map((x) => x.name);
  assert.ok(!names("top").includes("read"));
  assert.ok(names("top").includes("delegate"));
  assert.ok(names("main").includes("read") && !names("main").includes("write") && names("main").includes("push"));
  assert.ok(names("sub").includes("write") && !names("sub").includes("delegate"));
});

test("a read-only main refuses write even if asked by name", async () => {
  const turn: Turn = async (c) => { assert.equal(c.readOnly, true); assert.ok(!c.tools.some((t) => t.name === "write")); return "ok"; };
  const s = new Session(row(1, "main"), file(), turn, hooks());
  s.receive("person", "write a file");
  await s.turn();
});

test("delegate goes through the hook and the answer is the tool's text", async () => {
  const calls: string[] = [];
  const turn: Turn = async (c) => c.tools.find((t) => t.name === "delegate")!.run(c.cwd, { target: "", instruction: "do it" }, { user: c.user });
  const s = new Session(row(1, "main"), file(), turn, hooks({ delegate: async (_s, target, instruction) => { calls.push(`${target}|${instruction}`); return "delegated to 2"; } }));
  s.receive("person", "go");
  await s.turn();
  assert.deepEqual(calls, ["|do it"]);
  assert.equal((readRows(s.file).at(-1) as { answer: string }).answer, "delegated to 2");
});

test("abort marks the open turn interrupted and clears running", async () => {
  let release!: () => void;
  const s = new Session(row(1, "sub"), file(), (c) => new Promise((r) => { release = () => r("late"); c.signal.addEventListener("abort", () => r("aborted")); }), hooks());
  s.receive("person", "x");
  const p = s.turn();
  await new Promise((r) => setTimeout(r, 10));
  s.abort();
  await p;
  const kinds = readRows(s.file).map((r) => r.kind);
  assert.ok(kinds.includes("interrupted"));
  assert.ok(!kinds.includes("turn.end"));
  assert.equal(s.running, false);
  release();
});
```

- [ ] **Step 2: Run, expect failure**

Run: `cd harness/bench && node --test test/session.test.ts`

- [ ] **Step 3: Implement**

`harness/bench/src/session.ts`:

```ts
// One actor per session row (spec §2). It owns exactly one append-only log and runs one turn at a
// time; everything it knows between turns is re-read from the rows, so a restart loses nothing but
// the turn that was in flight, which boot marks `interrupted` and never resumes.
import { TOOLS, type Tool, type User, type RunCtx } from "./engine/index.ts";
import { append, readRows, unread, nextTurn, openTurn, type Row } from "./rows.ts";
import type { SessionRow } from "./sessions.ts";

export type TurnCtx = { prompt: string; cwd: string; tools: Tool[]; user: User; readOnly: boolean; log: (step: string) => void; signal: AbortSignal; history: Row[] };
export type Turn = (ctx: TurnCtx) => Promise<string>;
export type Hooks = {
  delegate: (from: Session, target: string, instruction: string) => Promise<string>;
  push?: (from: Session) => Promise<string>;
  tell: (from: Session, text: string) => void;
  askPerson: (from: Session, question: string) => Promise<string>;
};

// Tiers are tool lists, not prompt text (ruling 3). Main reads and reviews; only a sub changes a tree.
const TALK = ["tell_user", "ask_user", "think", "recall"];
export const TIER_TOOLS: Record<"top" | "main" | "sub", string[] | "all"> = {
  top: [...TALK, "delegate"],
  main: [...TALK, "delegate", "push", "read", "glob", "grep", "bash_output"],
  sub: "all",
};

const benchTool = (name: string, description: string, params: { name: string; description: string }[], run: Tool["run"]): Tool =>
  ({ name, description, brief: description.slice(0, 60), params: params.map((p) => ({ ...p, kind: "free" as const })), outcomes: ["done", "failed"], run });

export class Session {
  running = false;
  onAnswer?: (s: Session, end: Extract<Row, { kind: "turn.end" }>) => Promise<void>;
  private ctl?: AbortController;
  constructor(public readonly row: SessionRow, public readonly file: string, private turnFn: Turn, private hooks: Hooks) {}

  rows() { return readRows(this.file); }
  hasUnread() { return unread(this.rows()).length > 0; }
  receive(from: "person" | number, text: string, childTurn?: number) { append(this.file, { kind: "user", ts: Date.now(), from, text, ...(childTurn === undefined ? {} : { childTurn }) }); }

  tools(): Tool[] {
    const tier = this.row.tier ?? "main";
    const own: Tool[] = [
      benchTool("delegate", "Hand an instruction to another session and carry on; its answer arrives as a later message. target: a main's workspace name (from top), empty for a new sub or an open child's seq (from main).",
        [{ name: "target", description: "workspace name, child seq, or empty" }, { name: "instruction", description: "what the child should do" }],
        (_cwd, a) => this.hooks.delegate(this, a.target ?? "", a.instruction ?? "")),
      benchTool("recall", "Earlier turns of this session, oldest first.", [{ name: "turns", description: "how many turns back" }],
        async (_cwd, a) => this.rows().filter((r) => r.kind === "user" || r.kind === "turn.end").slice(-2 * (Number(a.turns) || 5)).map((r) => (r.kind === "user" ? `> ${r.text}` : r.answer ?? r.error ?? "")).join("\n")),
    ];
    if (tier === "main" && this.hooks.push) own.push(benchTool("push", "Push this workspace's working branch to the platform repo.", [], () => this.hooks.push!(this)));
    const allow = TIER_TOOLS[tier];
    const engine = TOOLS.filter((t) => t.name !== "recall" && (allow === "all" || allow.includes(t.name)));
    const ownAllowed = own.filter((t) => allow === "all" ? t.name !== "delegate" && t.name !== "push" : allow.includes(t.name));
    return [...ownAllowed, ...engine];
  }

  async turn() {
    if (this.running) return;
    const rows = this.rows();
    const pending = unread(rows);
    if (pending.length === 0) return;
    this.running = true;
    this.ctl = new AbortController();
    const turn = nextTurn(rows);
    append(this.file, { kind: "turn.start", ts: Date.now(), turn });
    const prompt = pending.map((r) => (r.from === "person" ? r.text : `[from session ${r.from}]\n${r.text}`)).join("\n\n");
    const user: User = { tell: (m) => this.hooks.tell(this, m), ask: (q) => this.hooks.askPerson(this, q) };
    let end: Extract<Row, { kind: "turn.end" }>;
    try {
      const answer = await this.turnFn({ prompt, cwd: this.row.workspace ?? `bench-${this.row.seq}`, tools: this.tools(), user, readOnly: this.row.tier !== "sub", log: (step) => append(this.file, { kind: "turn.step", ts: Date.now(), turn, step }), signal: this.ctl.signal, history: rows });
      if (this.ctl.signal.aborted) return;
      end = { kind: "turn.end", ts: Date.now(), turn, answer };
    } catch (e) {
      if (this.ctl.signal.aborted) return;
      end = { kind: "turn.end", ts: Date.now(), turn, error: (e as Error).message };
    } finally {
      this.running = false;
    }
    append(this.file, end);
    if (end.answer !== undefined && this.onAnswer) await this.onAnswer(this, end);
  }

  abort() {
    const turn = openTurn(this.rows());
    if (turn !== undefined) append(this.file, { kind: "interrupted", ts: Date.now(), turn });
    this.ctl?.abort();
    this.running = false;
  }
}
```

Note: an engine `error` leaves the `user` rows before it unread only if `unread()` counts from the last `turn.end` — it does, and an error end is a `turn.end`. Fix: in `unread()` (rows.ts) treat a `turn.end` with `error` as NOT a boundary. Update `rows.ts` and its test ("an error end leaves the rows unread") in this task.

- [ ] **Step 4: Run, expect pass**

Run: `cd harness/bench && node --test test/session.test.ts test/rows.test.ts`

- [ ] **Step 5: Commit**

```bash
git add harness/bench/src/session.ts harness/bench/src/rows.ts harness/bench/test/session.test.ts harness/bench/test/rows.test.ts
git commit -m "Add the session actor with tier tool lists"
```

---

### Task 7: Scheduler, boot repair, cap

**Files:**
- Create: `harness/bench/src/scheduler.ts`
- Test: `harness/bench/test/scheduler.test.ts`

**Interfaces:**
- Consumes: `Session`, `SessionList`, `rows.ts`.
- Produces:

```ts
export const MAX_RUNNING = 8;
export class Scheduler {
  constructor(list: SessionList, make: (row: SessionRow) => Session, max = MAX_RUNNING);
  sessions: Map<number, Session>;          // by seq, open rows only get an actor
  boot(): void;                             // interrupted rows, crash repair, then kick()
  kick(): void;                             // start turns for waiting sessions up to the cap (log order = seq order)
  get(seq: number): Session | undefined;
  deliver(child: Session, end: Extract<Row, { kind: "turn.end" }>): void;   // parent user row from:child, childTurn=end.turn; then kick()
}
```

- [ ] **Step 1: Failing tests**

`harness/bench/test/scheduler.test.ts`:

```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Scheduler } from "../src/scheduler.ts";
import { Session, type Hooks, type Turn } from "../src/session.ts";
import { SessionList } from "../src/sessions.ts";
import { append, readRows } from "../src/rows.ts";

const hooks: Hooks = { delegate: async () => "d", tell: () => {}, askPerson: async () => "" };
const setup = (turn: Turn, max?: number) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "sched-"));
  const list = new SessionList(dir);
  const sched = new Scheduler(list, (row) => new Session(row, list.logFile(row), turn, hooks), max);
  return { dir, list, sched };
};
const tick = () => new Promise((r) => setTimeout(r, 20));

test("boot marks an open turn interrupted and does not resume it", async () => {
  const ran: string[] = [];
  const { list, sched } = setup(async (c) => { ran.push(c.prompt); return "ok"; });
  const s = list.create(undefined, { tier: "main", state: "open", workspace: "w" });
  append(list.logFile(s), { kind: "user", ts: 1, from: "person", text: "old" });
  append(list.logFile(s), { kind: "turn.start", ts: 2, turn: 1 });
  sched.boot();
  await tick();
  const kinds = readRows(list.logFile(s)).map((r) => r.kind);
  assert.deepEqual(kinds.slice(0, 3), ["user", "turn.start", "interrupted"]);
  // the user row is still unread, so the scheduler runs a NEW turn 2 for it — but never re-runs turn 1
  assert.deepEqual(ran, ["old"]);
  assert.ok(kinds.includes("turn.end"));
});

test("boot re-delivers a child answer whose parent user row is missing", async () => {
  const { list, sched } = setup(async () => "ok");
  const main = list.create(undefined, { tier: "main", state: "open", workspace: "m" });
  const sub = list.create(undefined, { tier: "sub", state: "open", parent: main.seq, workspace: "c" });
  append(list.logFile(sub), { kind: "user", ts: 1, from: main.seq, text: "do" });
  append(list.logFile(sub), { kind: "turn.start", ts: 2, turn: 1 });
  append(list.logFile(sub), { kind: "turn.end", ts: 3, turn: 1, answer: "did it" });
  sched.boot();
  await tick();
  const parent = readRows(list.logFile(main));
  const got = parent.find((r) => r.kind === "user" && r.from === sub.seq && r.childTurn === 1);
  assert.ok(got, "parent got the child's answer");
  sched.boot(); // idempotent: matched by child seq and turn index
  assert.equal(readRows(list.logFile(main)).filter((r) => r.kind === "user").length, 1);
});

test("the cap holds and waiting sessions run in seq order", async () => {
  let live = 0, peak = 0;
  const order: number[] = [];
  const gates: (() => void)[] = [];
  const { list, sched } = setup((c) => new Promise((r) => { live++; peak = Math.max(peak, live); order.push(Number(c.prompt)); gates.push(() => { live--; r("ok"); }); }), 2);
  for (let i = 1; i <= 4; i++) { const s = list.create(undefined, { tier: "main", state: "open", workspace: `w${i}` }); append(list.logFile(s), { kind: "user", ts: i, from: "person", text: String(s.seq) }); }
  sched.boot();
  await tick();
  assert.equal(peak, 2);
  gates.shift()!(); await tick();
  gates.shift()!(); await tick();
  gates.shift()!(); gates.shift()!(); await tick();
  assert.deepEqual(order, [1, 2, 3, 4]);
});

test("a closed session never runs", async () => {
  const ran: string[] = [];
  const { list, sched } = setup(async (c) => { ran.push(c.prompt); return "ok"; });
  const s = list.create(undefined, { tier: "sub", state: "closed", workspace: "w" });
  append(list.logFile(s), { kind: "user", ts: 1, from: "person", text: "x" });
  sched.boot();
  await tick();
  assert.deepEqual(ran, []);
});
```

- [ ] **Step 2: Run, expect failure**

- [ ] **Step 3: Implement**

`harness/bench/src/scheduler.ts`:

```ts
// The one loop (spec §2): on boot, on every inbound write and when a turn ends, start a turn for
// every open session with unread rows, oldest seq first, never more than MAX_RUNNING at once.
// Boot also repairs the two two-write orders from spec §1.
import { append, openTurn, readRows, lastEnd, type Row } from "./rows.ts";
import type { Session } from "./session.ts";
import type { SessionList, SessionRow } from "./sessions.ts";

export const MAX_RUNNING = 8; // ponytail: one fixed cap; per-tier caps if top sessions starve

export class Scheduler {
  sessions = new Map<number, Session>();
  constructor(private list: SessionList, private make: (row: SessionRow) => Session, private max = MAX_RUNNING) {}

  get(seq: number) { return this.sessions.get(seq); }

  private actor(row: SessionRow): Session {
    let s = this.sessions.get(row.seq);
    if (!s) { s = this.make(row); s.onAnswer = async (c, end) => this.deliver(c, end); this.sessions.set(row.seq, s); }
    return s;
  }

  boot() {
    for (const row of this.list.all()) {
      const file = this.list.logFile(row);
      const rows = readRows(file);
      const open = openTurn(rows);
      if (open !== undefined) {
        append(file, { kind: "interrupted", ts: Date.now(), turn: open });
        // The parent decides what happens to an interrupted child (spec §5): told once, never resumed.
        const p = row.parent !== undefined ? this.list.bySeq(row.parent) : undefined;
        if (p) append(this.list.logFile(p), { kind: "user", ts: Date.now(), from: row.seq, text: `child ${row.seq} was interrupted by a bench restart; delegate to it again to resume on its clone, or leave it` });
      }
      // A child whose newest answer never reached its parent: the crash fell between the two writes.
      if (row.parent !== undefined) {
        const end = lastEnd(rows);
        const parent = this.list.bySeq(row.parent);
        if (end?.answer !== undefined && parent && !readRows(this.list.logFile(parent)).some((r) => r.kind === "user" && r.from === row.seq && r.childTurn === end.turn)) {
          append(this.list.logFile(parent), { kind: "user", ts: Date.now(), from: row.seq, text: end.answer, childTurn: end.turn });
        }
      }
    }
    this.kick();
  }

  kick() {
    const running = [...this.sessions.values()].filter((s) => s.running).length;
    let slots = this.max - running;
    for (const row of this.list.all().sort((a, b) => a.seq - b.seq)) {
      if (slots <= 0) break;
      if (row.state === "closed" || row.archived) continue;
      const s = this.actor(row);
      if (s.running || !s.hasUnread()) continue;
      slots--;
      void s.turn().finally(() => this.kick());
    }
  }

  deliver(child: Session, end: Extract<Row, { kind: "turn.end" }>) {
    const parent = child.row.parent !== undefined ? this.list.bySeq(child.row.parent) : undefined;
    if (parent && end.answer !== undefined) this.actor(parent).receive(child.row.seq, end.answer, end.turn);
    this.kick();
  }
}
```

- [ ] **Step 4: Run, expect pass**

Run: `cd harness/bench && node --test test/scheduler.test.ts`

- [ ] **Step 5: Commit**

```bash
git add harness/bench/src/scheduler.ts harness/bench/test/scheduler.test.ts
git commit -m "Add the session scheduler with boot repair and a run cap"
```

---

### Task 8: Sub lifecycle

**Files:**
- Create: `harness/bench/src/sub.ts`
- Test: `harness/bench/test/sub.test.ts`

**Interfaces:**
- Consumes: `Platform` (Task 4), `Scheduler`, `Session`, `remote` (Task 5), `SessionList`.
- Produces:

```ts
export class Subs {
  constructor(list: SessionList, sched: Scheduler, platform: Platform, backendFor: (ws: string) => Promise<void>);
  // main's delegate: "" spawns, an open child seq answers that child
  delegate(from: Session, target: string, instruction: string): Promise<string>;
  // scheduler onAnswer for a sub: push, deliver (parent user row with commit), delete clone, close row
  finish(child: Session, end: Extract<Row, { kind: "turn.end" }>): Promise<void>;
}
```

Push command run in the clone through `remote(clone, "exec", …)`:

```
git push -o receive.denyCurrentBranch=updateInstead ssh://kl@{mainIP}/home/kl/workspaces/{name} HEAD:{branch}
```

(`-o` is a push option and does not set receiver config; the receiver-side config is Task 11. Keep only `git push ssh://kl@{mainIP}/home/kl/workspaces/{name} HEAD:{branch}` — drop `-o`.) `{branch}` = main's working branch, read once at spawn via `remote(main, "exec", {cmd: "git rev-parse --abbrev-ref HEAD"})` and stored on the sub's row as `target` (the existing field). `{mainIP}` = `platform.tools(main.workspace)` minus `:7788`.

- [ ] **Step 1: Failing tests**

`harness/bench/test/sub.test.ts`:

```ts
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
  const sched = new Scheduler(list, (row) => new Session(row, list.logFile(row), async (c) => (row.tier === "sub" ? childAnswer : c.tools.find((t) => t.name === "delegate")!.run(c.cwd, { target: "", instruction: "build it" }, {})), hooks));
  const subs = new Subs(list, sched, platform, async (ws) => setBackend(ws, httpBackend(await platform.tools(ws))));
  const main = list.create(undefined, { tier: "main", state: "open", workspace: "m" });
  return { list, sched, subs, main };
};
const settle = () => new Promise((r) => setTimeout(r, 60));

test("delegate clones, writes child user row then parent delegate row, and the child runs", async () => {
  pushes = 0; deleted.length = 0;
  const { list, sched, main } = world();
  sched.get(main.seq) ?? sched.boot();
  sched.actorFor?.(main); // if exposed; else boot() above created it
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
  assert.match(clonePod.execs.find((c) => c.startsWith("git push"))!, /ssh:\/\/kl@127\.0\.0\.1\/home\/kl\/m HEAD:feat\/x/);
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
```

(The first test's `sched.actorFor?.(main)` line is scaffolding to delete once the implementer confirms `boot()` creates actors for every open row — it does, per Task 7's `kick()`; delete both lines and keep `sched.boot()`.)

- [ ] **Step 2: Run, expect failure**

- [ ] **Step 3: Implement**

`harness/bench/src/sub.ts`:

```ts
// The sub lifecycle (spec §4). Every step is a log row before the next side effect, so a crash
// resumes at the right step on boot: a child row with no clone id is re-spawned by hand, a child
// answer with no parent row is re-delivered by the scheduler, and a closed child is never touched.
import { Platform, PlatformError } from "./platform.ts";
import { remote, text } from "./engine/remote.ts";
import { append, readRows, type Row } from "./rows.ts";
import type { Scheduler } from "./scheduler.ts";
import type { Session } from "./session.ts";
import type { SessionList } from "./sessions.ts";

const NON_FF = /non-fast-forward|fetch first|rejected/;

export class Subs {
  constructor(private list: SessionList, private sched: Scheduler, private platform: Platform, private backendFor: (ws: string) => Promise<void>) {}

  async delegate(from: Session, target: string, instruction: string): Promise<string> {
    if (from.row.tier === "top") return this.toMain(from, target, instruction);
    if (target) {
      const child = this.list.bySeq(Number(target));
      if (!child || child.parent !== from.row.seq || child.state !== "open") return `error: no open child ${target}; open children: ${this.list.children(from.row.seq).filter((c) => c.state === "open").map((c) => c.seq).join(", ") || "none"}`;
      this.sched.get(child.seq)!.receive(from.row.seq, instruction);
      append(from.file, { kind: "delegate", ts: Date.now(), turn: -1, child: child.seq, instruction });
      this.sched.kick();
      return `delegated to ${child.seq}, waiting`;
    }
    const mainWs = from.row.workspace!;
    let branch: string, clone: string;
    try {
      await this.backendFor(mainWs);
      branch = text(await remote(mainWs, "exec", { cmd: "git rev-parse --abbrev-ref HEAD" })).trim();
      const seq = this.list.all().length + 1; // ponytail: the row's real seq is assigned on create; the clone name only needs to be unique
      clone = await this.platform.clone(mainWs, `sub-${seq}-${Date.now().toString(36)}`);
    } catch (e) {
      return `error: ${e instanceof PlatformError ? e.body : (e as Error).message}`; // a refused clone is the answer, verbatim (spec §5)
    }
    const row = this.list.create(from.row.model, { tier: "sub", state: "open", parent: from.row.seq, workspace: clone, target: branch, kind: "workspace" });
    await this.backendFor(clone);
    const child = this.sched.get(row.seq) ?? (this.sched.kick(), this.sched.get(row.seq)!);
    child.receive(from.row.seq, `${instruction}\n\nWork on branch sub/${row.seq}; commit your changes there. Your result is pushed into the main workspace when you finish.`);
    append(from.file, { kind: "delegate", ts: Date.now(), turn: -1, child: row.seq, instruction });
    child.onAnswer = (c, end) => this.finish(c, end);
    this.sched.kick();
    return `delegated to ${row.seq}, waiting`;
  }

  private toMain(from: Session, name: string, instruction: string) {
    const mains = this.list.all().filter((s) => s.tier === "main" && s.state !== "closed");
    const m = mains.find((s) => s.workspace === name || s.name === name);
    if (!m) return `error: no main named ${name}; mains: ${mains.map((s) => s.workspace).join(", ") || "none"}`;
    this.sched.get(m.seq)!.receive(from.row.seq, instruction);
    append(from.file, { kind: "delegate", ts: Date.now(), turn: -1, child: m.seq, instruction });
    this.sched.kick();
    return `delegated to ${m.workspace}, waiting`;
  }

  async finish(child: Session, end: Extract<Row, { kind: "turn.end" }>) {
    const row = child.row;
    const parent = this.list.bySeq(row.parent!)!;
    const clone = row.workspace!, mainWs = parent.workspace!;
    const mainIp = (await this.platform.tools(mainWs)).replace(/:\d+$/, "");
    const mainName = await this.platform.name(mainWs);
    const push = async () => text(await remote(clone, "exec", { cmd: `git push ssh://kl@${mainIp}/home/kl/workspaces/${mainName} HEAD:${row.target}`, timeout_ms: 120_000 }));
    let out = await push();
    const retried = readRows(child.file).some((r) => r.kind === "user" && r.text.startsWith("rebase onto main"));
    if (NON_FF.test(out) && !retried) {
      // main moved under us: one retry through the child, then it reports failure itself
      child.receive(row.parent!, `rebase onto main and push again: the push was rejected.\n${out}`);
      child.onAnswer = (c, e) => this.finish(c, e);
      this.sched.kick();
      return;
    }
    const commit = text(await remote(clone, "exec", { cmd: "git rev-parse HEAD" })).trim();
    const pushed = !/exit \d/.test(out);
    const answer = pushed ? `${end.answer}\n\npushed ${commit} to ${row.target}` : `${end.answer}\n\npush failed:\n${out}`;
    this.sched.get(parent.seq)!.receive(row.seq, answer, end.turn);      // step 4 before step 5: the answer is never lost to a crash
    try { await this.platform.remove(clone); } catch (e) { this.sched.get(parent.seq)!.receive(row.seq, `clone ${clone} could not be deleted: ${(e as Error).message}`); }
    this.list.update(row.id, { state: "closed" });
    this.sched.kick();
  }
}
```

`turn: -1` on `delegate` rows: the `Session.turn()` context does not expose the turn index to hooks. Add `current?: number` to `Session` set in `turn()` and use `from.current ?? -1` here (three-line change in `session.ts`).

- [ ] **Step 4: Run, expect pass**

Run: `cd harness/bench && node --test test/sub.test.ts`

- [ ] **Step 5: Commit**

```bash
git add harness/bench/src/sub.ts harness/bench/src/session.ts harness/bench/test/sub.test.ts
git commit -m "Add the sub lifecycle: clone, push into main, deliver, delete"
```

---

### Task 9: Wire the bench: replace RpcChild

**Files:**
- Modify: `harness/bench/src/bench.ts`, `harness/bench/src/server.ts`, `harness/bench/src/main.ts`
- Delete: `harness/bench/src/rpc-child.ts`, `harness/bench/test/rpc-child.test.ts`, `harness/bench/test/fake-pi.ts`
- Test: `harness/bench/test/tree.test.ts`; existing `bench.test.ts`, `server.test.ts` updated

**Interfaces:**
- Consumes: `Scheduler`, `Session`, `Subs`, `Platform`, `runtime.ts` (Task 10 — until then `Bench` takes `turn: Turn` in its options; production passes `makeTurn()`).
- Produces on `Bench`: `send(id, text)` (person → session, returns `{turn}`), `children(id)`, `rows(id, after?, limit?)`; `create(tier?: "top"|"main")`; `abort(id)`. `messages(id, …)` stays and returns the rows (the electron UI keeps reading it; its shape change is the frontend's problem, out of scope). Emits `{type: "row", session: id, row}` on `/events` for every append.

- [ ] **Step 1: Read `bench.ts` fully and list every `RpcChild`/`PiEvent`/`turning` use**

`grep -n "RpcChild\|PiEvent\|turning\|children\." harness/bench/src/bench.ts` — every line is replaced in Step 3.

- [ ] **Step 2: Failing test**

`harness/bench/test/tree.test.ts`:

```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";

test("top delegates to a main by workspace name; rows flow on events", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-"));
  const events: unknown[] = [];
  const bench = new Bench({ dir, readOnly: false, turn: async (c) => (c.tools.some((t) => t.name === "read") ? "main did it" : c.tools.find((t) => t.name === "delegate")!.run(c.cwd, { target: "api", instruction: "ship" }, {})), platform: undefined });
  bench.on((e) => events.push(e));
  await bench.start();
  const top = await bench.create("top");
  const main = await bench.openWorkspace("api");
  await bench.send(top.id, "ship the api");
  await new Promise((r) => setTimeout(r, 60));
  const rows = await bench.rows(main.id);
  assert.ok(rows.rows.some((r: { kind: string; from?: unknown }) => r.kind === "user" && r.from === top.seq));
  assert.ok(rows.rows.some((r: { kind: string; answer?: string }) => r.kind === "turn.end" && r.answer === "main did it"));
  assert.ok((await bench.rows(top.id)).rows.some((r: { kind: string; from?: unknown }) => r.kind === "user" && r.from === main.seq));
  assert.ok(events.some((e) => (e as { type: string }).type === "row"));
  assert.deepEqual((await bench.children(top.id)).map((s) => s.tier), []); // delegation to a main is not parentage
  await bench.stop();
});
```

(`bench.on` — use whatever subscription method `Bench` already exposes for `/events`; grep `emit(` and its listener registration.)

- [ ] **Step 3: Implement in `bench.ts`**

- Options gain `turn: Turn` and `platform?: Platform`. Drop `bin`, `model` stays.
- Fields: replace `children: Map<string, RpcChild>` and `turning` with `sched: Scheduler` and `subs: Subs`. `Session` hooks: `delegate: (s,t,i) => this.subs.delegate(s,t,i)`, `tell: (s, m) => this.emit({type:"row", session: s.row.id, row: {kind:"tell", text: m}})`, `askPerson`: for a row with `parent` set, append the question as a parent `user` row `from: seq` and return `"asked the parent; its answer arrives as a message"` at once (the child's turn ends; the parent's `delegate(childSeq, …)` is the answer, spec §4). For a row without a parent, append a `turn.step` `ask:{q}` row and resolve with the next `user` row from the person (store a pending resolver per session; `send()` resolves it if present instead of appending a user row).
- `start()`: `this.sched.boot()`. `stop()`: abort every running session (`s.abort()`), no processes to kill.
- `create(tier = "main")`: one top per bench — refuse a second with `Error("this bench already has a top session")`; row `{tier, state:"open", kind: tier === "top" ? "bench" : "workspace"}`.
- `openWorkspace(ws)` keeps its id scheme and sets `tier: "main", state: "open", workspace: ws`; after create, `await backendFor(ws)` when `platform` is set (`setBackend(ws, httpBackend(await platform.tools(ws)))`; without a platform (tests) leave the backend unset).
- `send(id, text)`: refuse closed/archived (409 wording "session {id} is closed"); `actor.receive("person", text)`; `sched.kick()`; return `{ turn: nextTurn }`.
- `rpc(id, cmd)`: keep the route alive for the electron app: `prompt` → `send`, `abort` → `abort`, anything else → `Error("rpc {type} is gone; the bench runs the sys-1 engine")`.
- `messages(id)`/`rows(id)`: `page(readRows(file), after, limit)` shaped `{ rows, total }` (keep `messages` key too for the old UI: `{messages: rows, rows, total}`).
- `abort(id)`: `sched.get(seq)?.abort()`.
- `children(id)`: `list.children(seq)`.
- `remove(id)`: also abort and, for a sub with an open clone, leave the clone (deleting a workspace is a person's explicit act; note in a comment).
- Every `append` goes through one `Bench.append(session, row)` that also emits `{type:"row", session, row}` — pass `emit` into `Session` via a fourth constructor arg `onRow?: (row: Row) => void`, called after each `append` in `session.ts` (add the field; three call sites).

`server.ts`: add `GET /sessions/{id}/children` → `bench.children(id)`; `POST /sessions/{id}/send {text}` → `bench.send`; `POST /sessions/{id}/abort`. WS `/sessions/{id}/rpc` stays, now backed by the shim above. Error map: add `/is closed/ → 409`.

`main.ts`: build `new Bench({ dir, readOnly, model: a.model, turn: makeTurn(), platform: Platform.fromEnv() })` — `makeTurn` from Task 10; until Task 10 lands, import a placeholder that throws "engine not wired" so the binary still starts read-only. Remove `--bin`/pi options.

- [ ] **Step 4: Delete the pi files and fix the tests**

```bash
git rm harness/bench/src/rpc-child.ts harness/bench/test/rpc-child.test.ts harness/bench/test/fake-pi.ts
```

Update `bench.test.ts`, `server.test.ts`, `main.test.ts`, `idle.test.ts`, `reschedule.test.ts`, `server-threads.test.ts` wherever they construct `Bench` with `bin` or spawn fake-pi: pass `turn: async () => "ok"` and assert on rows instead of pi events. Tests that only tested pi process behaviour (respawn, `get_state`) are deleted with a one-line commit-body note each.

- [ ] **Step 5: Run the whole suite**

Run: `cd harness/bench && node --test 'test/*.test.ts' 2>&1 | tail -20`
Expected: all pass; `grep -rn "child_process" harness/bench/src` prints nothing.

- [ ] **Step 6: Commit**

```bash
git add -A harness/bench
git commit -m "Run sessions on the sys-1 engine in the bench process"
```

---

### Task 10: Production engine wiring

**Files:**
- Create: `harness/bench/src/runtime.ts`
- Test: `harness/bench/test/runtime.test.ts`

**Interfaces:**
- Produces: `export function makeTurn(env = process.env): Turn` — builds `runTask({ llm: makeAiSdkLlm(), ask, cwd: ctx.cwd, log: ctx.log, user: ctx.user, context: () => history, steer: () => ({ lines: [], stop: ctx.signal.aborted }) }, ctx.prompt, ctx.prompt.slice(0, 80))` where `runTask`'s tool list is `ctx.tools` (check how `runTask` takes tools: if it reads the module `TOOLS` constant, add a `tools?: Tool[]` field to `RunTaskDeps` and thread it into `makeAct`/`makePlanner` — grep `TOOLS` in `executor.ts`/`task.ts`; this is the one engine edit in this task) and `readOnly: ctx.readOnly`.

- [ ] **Step 1: Failing test**

`harness/bench/test/runtime.test.ts`:

```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import { makeTurn } from "../src/runtime.ts";

test("makeTurn refuses to start without the two credentials", () => {
  assert.throws(() => makeTurn({} as NodeJS.ProcessEnv), /TYPESAFE_API_KEY/);
  assert.throws(() => makeTurn({ TYPESAFE_API_KEY: "x" } as NodeJS.ProcessEnv), /JEVHARN_API_KEY/);
});
```

- [ ] **Step 2: Implement**

```ts
// Production seam between a Session and the engine: one runTask per turn, the log rows as context.
import { runTask, makeAiSdkLlm, ask } from "./engine/index.ts";
import type { Turn } from "./session.ts";

export function makeTurn(env = process.env): Turn {
  for (const k of ["TYPESAFE_API_KEY", "JEVHARN_API_KEY"]) if (!env[k]) throw new Error(`${k} is not set; the bench cannot run an engine`);
  const llm = makeAiSdkLlm();
  return (ctx) => {
    const history = ctx.history.filter((r) => r.kind === "user" || r.kind === "turn.end").slice(-10).map((r) => (r.kind === "user" ? `user: ${r.text}` : `main: ${r.answer ?? r.error ?? ""}`)).join("\n");
    return runTask({ llm, ask, cwd: ctx.cwd, log: ctx.log, user: ctx.user, context: () => history, steer: () => ({ lines: [], stop: ctx.signal.aborted }), tools: ctx.tools, readOnly: ctx.readOnly }, ctx.prompt, ctx.prompt.slice(0, 80));
  };
}
```

Then thread `tools` and `readOnly` through `RunTaskDeps` → `makeAct`'s `ActDeps` (`readOnly` exists there already) and the tool list used by the planner and the act picker (grep `TOOLS` under `engine/`; replace each read with `deps.tools ?? TOOLS`).

- [ ] **Step 3: Run**

Run: `cd harness/bench && node --test test/runtime.test.ts test/engine-loads.test.ts`

- [ ] **Step 4: Manual exercise (required before Task 11 — memory: edit → exercise → tests → ship)**

With real keys in the shell, against a live workspace `ws` whose tool server the laptop can reach through `kl-connect ws ide <ws>` (it forwards 7788 to a local port): set `KL_CONFIG_DIR`, start `node src/main.ts --dir /tmp/bench-x`, `POST /sessions` `{tier:"main"}` then `POST /sessions/{id}/send {"text":"list the files in this project"}`, and confirm `GET /sessions/{id}/messages` shows `turn.step` rows with a `glob` call and a `turn.end` answer. Paste the `turn.end` row into the commit body.

- [ ] **Step 5: Commit**

```bash
git add harness/bench/src/runtime.ts harness/bench/src/engine harness/bench/test/runtime.test.ts
git commit -m "Wire the sys-1 engine into a session turn"
```

---

### Task 11: Platform prerequisites for the clone push (Rust, dev pod)

**Files:**
- Modify: `crates/api/src/credentials.rs` (`authorized_keys_for`), the workspace prelude (grep `gitignore-global` under `crates/workspaces/src/k8s/` to find the prelude script)
- Test: `crates/api` unit test beside `authorized_keys_for`; `bins/agent/tests` prelude assertion if one exists

- [ ] **Step 1: Find both sites**

```bash
grep -n "fn authorized_keys_for" -A 30 crates/api/src/credentials.rs | head -50
grep -rn "gitignore-global" crates/workspaces/src/k8s/*.rs | head
grep -rn "fn rotate_user_key\|platform.*public\|id_ed25519" crates/api/src/*.rs crates/workspaces/src/k8s/*.rs | head
```

- [ ] **Step 2: Failing test for the key**

Beside the existing `authorized_keys_for` tests (same file, `#[cfg(test)]`):

```rust
#[test]
fn authorized_keys_include_the_owners_platform_key() {
    // the clone pod pushes into main's sshd with the platform key every pod mounts (spec: Facts)
    let keys = render_with_platform(&[], Some("ssh-ed25519 AAAAplatform kloudlite"));
    assert!(keys.contains("ssh-ed25519 AAAAplatform kloudlite\n"), "{keys}");
}
```

Adapt to the real function shape: if `authorized_keys_for` takes the directory and owner, factor the rendering into a pure `render_authorized_keys(user_keys: &[String], platform_key: Option<&str>) -> String` that the test calls, and have `authorized_keys_for` fetch the owner's platform public key (the same source `rotate_user_key` registers the fingerprint from; if only the private key is stored, derive the public with `ssh_key::PrivateKey::public_key()` — the crate is already a dependency if `rotate_user_key` mints ed25519 keys; check `Cargo.toml`).

- [ ] **Step 3: Implement, run**

`cargo test -p kloudlite-api credentials` in the dev pod. Expected: PASS.

- [ ] **Step 4: Prelude**

In the prelude script string, after the gitignore-global append, add:

```sh
git config --global receive.denyCurrentBranch updateInstead
```

with the comment: a sub's clone pushes straight onto main's checked-out branch (spec §4); safe because main never holds uncommitted edits. If a prelude test asserts the script text, extend it with this line.

- [ ] **Step 5: Clippy and tests**

In the dev pod: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -5` and `cargo test -p kloudlite-api -p kloudlite-workspaces 2>&1 | tail -5`.

- [ ] **Step 6: Exercise on the fleet before commit**

Ship with `deploy/dev/dev-push.sh --aks` (memory: fast dev loop), then from a clone pod's `exec`: `git push ssh://kl@<main pod IP>/home/kl/workspaces/<name> HEAD:<branch>` must succeed and `git -C /home/kl/workspaces/<name> log -1` on main must show the commit. Paste both lines into the commit body.

- [ ] **Step 7: Commit**

```bash
git add crates/api/src/credentials.rs crates/workspaces/src/k8s
git commit -m "Admit the platform key on every workspace and accept pushes onto the checked-out branch"
```

---

### Task 12: Fleet proof — `bench.delegate` probe

**Files:**
- Modify: `crates/workspaces/src/slo/catalogue.rs`, `deploy/slo.md`, the bench probe stage under `bins/slo/src/` (grep `bench.tunnel` for the file)

- [ ] **Step 1: Catalogue entry**

Add `bench.delegate` to the hourly suite with the target "top → main → sub: the push lands on main's branch, the clone is gone, the child is closed", mirrored verbatim in `deploy/slo.md` (the equality test tells you the exact wording rule).

- [ ] **Step 2: Stage**

In the bench stage, after `bench.tunnel`: create a main via `POST /sessions/workspaces/{ws}` on the bench, `POST /sessions/{top}/send {"text": "delegate to <ws>: create hello.txt containing hi and commit it"}`, poll `GET /sessions/{main}/children` until one child is `closed` (cap 10 min), then assert through the main workspace's tool server `exec git log -1 --format=%s` contains the commit, `GET /v1/workspaces/{clone}` is 404, and the child row `state == "closed"`. Judge on output, never on status alone (memory: probes judge output).

- [ ] **Step 3: Run in the pod and on the fleet**

`cargo test -p kloudlite-workspaces slo` (catalogue equality), then create the hourly job by hand (memory: run probes, don't wait) and read the run row.

- [ ] **Step 4: Commit**

```bash
git add crates/workspaces/src/slo/catalogue.rs deploy/slo.md bins/slo
git commit -m "Probe delegation end to end from the bench"
```

---

## Self-review

- **Spec coverage.** §1 rows/derivation/two-write/boot: Tasks 2, 6, 7. §2 actor/scheduler/tiers/abort/cap: 6, 7. §3 engine copy, HTTP tools, notes in workspace: 1, 5, 10. §4 lifecycle, prelude, authorized_keys, non-ff retry: 8, 11. §5 top→main by name, `children` route, errors verbatim, restart, tests, probe: 8, 9, 12. `recall` reads the session log (§3 says sections move to the bench log): Task 6. `ask_user` from a child goes to the parent: Task 6's `askPerson` hook — **gap**: Task 9 routes `askPerson` to the person for every session; for a sub or main with a parent it must instead `receive` into the parent and resolve on the parent's next `delegate` to that child. Both fixed inline: parent routing in Task 9 Step 3, the restart notice in Task 7 `boot()`.
- **Placeholders.** None of TBD/TODO. Task 9 Step 3 is prose-with-code by necessity (it edits a 350-line file the implementer must read); every behaviour named has its wording or signature.
- **Type consistency.** `Session(row, file, turnFn, hooks)` in 6, 7, 8, 9; `onRow` fourth arg added in 9 is optional, so 6–8's calls stand. `Scheduler(list, make, max)`; `Subs(list, sched, platform, backendFor)`; `Platform.tools/clone/remove`; `remote(cwd, name, args)` and `text()`; `Row` kinds identical across tasks; `TurnCtx.history: Row[]` used by `runtime.ts`. `Session.current` (Task 8) is set in `turn()`.
