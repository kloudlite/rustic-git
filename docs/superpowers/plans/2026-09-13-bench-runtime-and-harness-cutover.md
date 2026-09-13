# Bench runtime and harness cutover — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: use superpowers:subagent-driven-development (or superpowers:executing-plans) to implement this plan task by task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `harness-bench`, the supervisor that runs a person's bench sessions against a bench folder and serves them on port 7789, runs every workspace and ephemeral session beside them with its tools on the workspace's tool server, and answers the platform's probe and lock contract. Then cut the Electron harness over from spawning `pi` locally to talking to that server at `HARNESS_BENCH`.

**Spec:** `docs/superpowers/specs/2026-09-13-bench-sessions-server-side-design.md` (sections "What runs where", "The bench folder", "The bench process", "Sync across devices", "What the harness stops doing", "Failure behaviour", "Migration", and the unit half of "Verification").

**Out of scope (the Rust plan owns it):** the `Bench` CRD, the agent reconciler, `/v1/bench`, the tunnel JWT, the gateway route, and the SLO probes. This plan runs `harness-bench` by hand against a local folder. `HARNESS_BENCH=http://127.0.0.1:<port>` stands in for the tunnel.

**Architecture:** `harness/bench/` is a Node 24 ESM package, and the harness installs its dependencies (`ws`, the pi SDK). It runs its `.ts` directly through Node's built-in type stripping, so there is no build step. Tests use `node --test`.

- **One writer.** `harness-bench` is the only process that writes harness files in the folder. pi writes its own `sessions/*.jsonl`.
- **Extensions publish, the server records.** Extensions report exchanges and processes over the RPC `setWidget` channel they already use, and the server records them.
- **Electron stays API-stable.** The renderer keeps calling `window.harness.pi(cmd, id)`. Its implementation in `main.ts` becomes a WebSocket client.

**Tech stack:** Node 24.16 (`node:test`, type stripping), `ws` 8, `@mariozechner/pi-coding-agent` 0.73.1 (`SessionManager`, `buildSessionContext`), Electron main-process TypeScript, SolidJS renderer.

**Where to run:**

- **Location:** everything here is TypeScript under `harness/`. Build and run it on the Mac with npm. Never run cargo.
- **Commits:** edit and commit in the dev pod checkout at `/work/src`, as with all rustic-git code, and push to both `origin` and `platform`.
- **Tests:** tests run wherever `node` 24 and `harness/node_modules` exist. The lock tests need the `flock` binary: util-linux in the pod, `brew install flock` on the Mac.

---

## Decisions this plan makes (spec gaps)

1. **Extensions do not write the folder; `harness-bench` does.**
   - **The conflict:** the spec has `process.ts` replacing `procs.json` and `kloudlite.ts` appending `exchanges.jsonl`. But every open session is its own pi process with its own copy of each extension, so N processes would race on one atomically replaced `procs.json`. The spec also says "one process writes the folder".
   - **What the extensions do:** they publish over `ctx.ui.setWidget`, which already works for `harness:procs`. `process.ts` adds `pid` to its snapshot. `kloudlite.ts` publishes `harness:exchange`. Background task transitions are already visible as RPC events (`tool_execution_*`, the `background-task` custom message).
   - **What the server does:** it folds all of these into `procs.json`, `exchanges.jsonl` and `tasks.jsonl`.
2. **What counts as an exchange today.** Every `kl_workspace_*` or `kl_environment_*` tool call records an `out` row before the call and an `in` row with the result. The workspace is the call's `id` or `name` argument. There is no "send a message to a workspace" tool yet. When one lands, it publishes the same widget.
3. **`after=` is a message index**, the count of resolved messages a client already holds. It is not a timestamp: pi messages have no stable id across `get_messages`, and an index is what `replay` needs.
4. **Lost is a flag.** A lost process row gets `lost: true` plus `ended` set to the time it was found lost. A lost task gets `state: "lost"`. This keeps `ended` a number, as the renderer's `Proc` type has it.
5. **flock through the `flock(1)` binary.** Node has no flock, and a native addon is not worth adding.
   - **How it is held:** `harness-bench` spawns `flock -x [-n] /bench/.lock -c 'echo locked; exec cat'` with a pipe on stdin. The lock lives as long as that child, and the child lives as long as our stdin pipe, so the lock dies with `harness-bench`.
   - **The holder:** `.lock.holder` records `NODE_NAME` (the pod's node, from the downward API) or else the hostname, and the pid. It is what "waiting on …" and the exit-75 termination message name.
6. **Session ids stay `bench` and `s-N`**, and `/btw` answers get ids `btw-N`. Import preserves them. A new session is `s-(max seq + 1)`.
7. **`harness bench import` is a palette command plus `POST /import`.** A CLI cannot read Electron's `localStorage` (leveldb under `userData`), and the list is the thing being imported. The renderer sends the list, main attaches each remembered file's bytes plus every other `*.jsonl` in the local session dir, and the server merges idempotently.
8. **Offline cache lives in main**, at `userData/bench-cache.json`, keyed by the `HARNESS_BENCH` URL. It holds the list, the last 200 raw messages per open session, and the last 500 exchanges.
9. **Deleting the `bench` session.** On the laptop, the first session could not be deleted and was emptied in place. On the server, `bench` is an ordinary session, and deleting the last live session creates a fresh `s-N` first. The renderer's `stored()` check that `v[0].id === "bench"` goes away with `localStorage`.
10. **Workspace and ephemeral sessions are sessions of this bench** (Tasks 18–21).
    - **Ids and files:** a workspace's thread is `w-{ws}` at `workspaces/{ws}/thread.jsonl`; an ephemeral's is `e-{id}` at `workspaces/{ws}/eph/{id}.jsonl`. Both ids are DNS labels, checked before they become paths.
    - **The record:** `SessionRow` gains `kind`, `workspace` and `target`. `target` is the workspace id whose tool server runs the tools. It is the workspace itself, or the ephemeral: an ephemeral is a workspace cut for one agent, so its id is a workspace id. The address is never stored, because a pod's IP changes; the extension asks `/v1` for it.
    - **Tools:** such a session loads only `workspace-tools.ts` with `--tools read,write,edit,bash,grep,find,ls`. `background.ts` would take `bash` back into the bench pod, so it is not loaded.
    - **Counting:** "at least one open session" and "the only open session" count bench sessions only; a workspace thread never stands in for one.
11. **The platform's contract for the entry point** (platform plan decision 8 and Task 5).
    - **Listen:** `--host` defaults to `0.0.0.0`: the gateway dials the pod IP, and the platform's `bench-ingress` NetworkPolicy admits only the gateway. A laptop passes `--host 127.0.0.1`.
    - **Probe:** `--ping` fetches `http://127.0.0.1:<port>/healthz` and exits 0 or 1. It is the pod's readiness probe.
    - **Lock:** a held lock exits 75 by default and writes the holder to `/dev/termination-log` (`TERMINATION_LOG` overrides the path). The agent reads exit 75 as `FolderLocked` and the pod restarts until the lease frees. `--wait` waits instead, for a laptop.
    - **Read-only:** `--read-only` is a departed member's bench (platform plan decision 10), never a stopped one: history through pi's SDK, no pi child, no tool, no lock. A bench that should cost nothing has no pod at all.
12. **Idle is `harness-bench`'s to observe, and exiting is how it says so** (platform plan decision 6).
    - **What counts:** idle means no connected client and no running work. A client is an open WebSocket (`/events` or a session's `/rpc`); a plain HTTP request is too short to hold a bench up. Running work is a pi turn between `agent_start` and `agent_end` (or the child's exit), a task `running` or `background`, or a process without `ended`.
    - **The clock:** `Idle` (`src/idle.ts`) keeps `idleSince`, set the moment both counts reach zero and cleared by any client or work. `GET /healthz` reports `{clients, busy, idleSince}`.
    - **The exit:** `--idle-secs N` (default `KL_BENCH_IDLE_SECS`, else 0 = never; a laptop never sets it) checks every 5 s. Once `idleSince` is N seconds old, `main.ts` writes `idle` to the termination log, stops the children, releases the lock and exits 0. The agent reads a `Succeeded` pod as asleep and starts a new one when a client next connects through `/v1`.

---

## File structure

```
harness/bench/package.json          {"type":"module"}; scripts test/start
harness/bench/src/log.ts            appendLine (O_APPEND+fsync), readLines (torn-tail skip), replaceJson (tmp+rename)
harness/bench/src/lock.ts           takeLock(dir, {wait}) via flock(1)
harness/bench/src/sessions.ts       SessionList over sessions.json
harness/bench/src/exchanges.ts      ExchangeLog: one log, bySession/byWorkspace views, discard
harness/bench/src/ledger.ts         Tasks (tasks.jsonl fold, lost on start), Procs (procs.json, lost on start)
harness/bench/src/rpc-child.ts      RpcChild: pi --mode rpc over stdio (src/pi.ts without Electron)
harness/bench/src/reader.ts         transcript(file) / listFiles(dir) via the pi SDK, no agent
harness/bench/src/guard.ts          Writable: a failed write flips it; a probe beat restores it
harness/bench/src/bench.ts          Bench: sessions ⇄ children, event folding, delete/archive/restore/btw/import
harness/bench/src/idle.ts           Idle: connected clients + busy() → idleSince
harness/bench/src/server.ts         HTTP routes + WS /sessions/{id}/rpc + WS /events; counts clients into Idle
harness/bench/src/main.ts           CLI: --dir --host --port --read-only --wait --ping --idle-secs; lock (exit 75), reopen, listen, idle exit (0)
harness/bench/test/*.test.ts        node --test
harness/bench/test/fake-pi.ts       a pi stand-in speaking RPC, for tests
harness/pi/process.ts               + pid in the snapshot
harness/pi/kloudlite.ts             + harness:exchange widget around workspace calls; exports call()
harness/pi/workspace-tools.ts       a workspace session's read/write/edit/bash/grep/find/ls on the workspace's tool server
harness/src/bench-client.ts         main-process client: REST, WS per session, /events, cache, reconnect
harness/src/main.ts                 pi/pi:spawn/pi:stop IPC → bench-client; bench:* IPC
harness/src/preload.ts              + bench REST, onBench, connection state
harness/src/pi.ts                   deleted (moved to bench/src/rpc-child.ts)
harness/src/renderer/App.tsx        sessions from the bench; archive/restore/delete/new/btw/import through it
harness/src/renderer/live.ts        + setProcs/setTasks from snapshots; lost state; connected signal
harness/package.json                + ws dependency, bench:test / bench:start scripts
```

---

### Task 1: Bench package and the append-only log

**Files:**
- Create: `harness/bench/package.json`, `harness/bench/src/log.ts`, `harness/bench/test/log.test.ts`
- Modify: `harness/package.json` (scripts, `ws` dependency)

**Interfaces:**
```ts
export function appendLine(file: string, value: unknown): void;           // throws on any write/fsync failure
export function readLines<T = unknown>(file: string): T[];                // [] when absent; skips a torn LAST line only
export function replaceJson(file: string, value: unknown): void;          // tmp in same dir, fsync, rename
export function readJson<T>(file: string, fallback: T): T;
```

- [ ] **Step 1: Create the package**

`harness/bench/package.json`:
```json
{
  "name": "harness-bench",
  "private": true,
  "type": "module",
  "scripts": {
    "test": "node --test test/",
    "start": "node src/main.ts"
  }
}
```

In `harness/package.json`, add to `scripts`:
```json
"bench:test": "node --test bench/test/",
"bench:start": "node bench/src/main.ts"
```
Then add `ws` as a direct dependency. It is already installed transitively.

Run: `cd harness && npm install --save ws@8.21.3 && npm install --save-dev @types/ws`
Expected: `package.json` lists `"ws": "^8.21.3"`, and the lockfile changes without errors.

- [ ] **Step 2: Write the failing test**

`harness/bench/test/log.test.ts`:
```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { appendLine, readLines, replaceJson, readJson } from "../src/log.ts";

const tmp = () => fs.mkdtempSync(path.join(os.tmpdir(), "bench-log-"));

test("appended lines read back in order", () => {
  const f = path.join(tmp(), "a.jsonl");
  appendLine(f, { n: 1 });
  appendLine(f, { n: 2 });
  assert.deepEqual(readLines(f), [{ n: 1 }, { n: 2 }]);
});

test("a torn last line is skipped, earlier lines survive", () => {
  const f = path.join(tmp(), "a.jsonl");
  appendLine(f, { n: 1 });
  fs.appendFileSync(f, '{"n":2,"tex');
  assert.deepEqual(readLines(f), [{ n: 1 }]);
  appendLine(f, { n: 3 });
  // The torn fragment is now a middle line: still unreadable, still skipped,
  // and the line after it is intact because appendLine starts on a fresh line.
  assert.deepEqual(readLines(f), [{ n: 1 }, { n: 3 }]);
});

test("a missing file is empty, not an error", () => {
  assert.deepEqual(readLines(path.join(tmp(), "none.jsonl")), []);
});

test("replaceJson is atomic: no temp file left, old content until rename", () => {
  const d = tmp();
  const f = path.join(d, "sessions.json");
  replaceJson(f, [{ id: "bench" }]);
  replaceJson(f, [{ id: "bench" }, { id: "s-2" }]);
  assert.deepEqual(readJson(f, []), [{ id: "bench" }, { id: "s-2" }]);
  assert.deepEqual(fs.readdirSync(d), ["sessions.json"]);
});
```

- [ ] **Step 3: Run it and watch it fail**

Run: `cd harness && npm run bench:test`
Expected: FAIL with `Cannot find module '.../bench/src/log.ts'`.

- [ ] **Step 4: Implement**

`harness/bench/src/log.ts`:
```ts
import fs from "node:fs";
import path from "node:path";

/**
 * The bench folder's two write shapes. Logs are append-only, one JSON line per
 * write, O_APPEND + fsync: a crash loses at most the line being written. A
 * list is small and replaced whole through a temp file and rename, so a reader
 * sees the old list or the new one, never half of either.
 */
export function appendLine(file: string, value: unknown): void {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const fd = fs.openSync(file, "a");
  try {
    // A torn previous line has no LF; start ours on a fresh line so the torn
    // fragment stays one unreadable line instead of corrupting this one.
    const size = fs.fstatSync(fd).size;
    let lead = "";
    if (size > 0) {
      const b = Buffer.alloc(1);
      const rfd = fs.openSync(file, "r");
      try { fs.readSync(rfd, b, 0, 1, size - 1); } finally { fs.closeSync(rfd); }
      if (b[0] !== 0x0a) lead = "\n";
    }
    fs.writeSync(fd, lead + JSON.stringify(value) + "\n");
    fs.fsyncSync(fd);
  } finally {
    fs.closeSync(fd);
  }
}

export function readLines<T = unknown>(file: string): T[] {
  let text: string;
  try {
    text = fs.readFileSync(file, "utf8");
  } catch (e) {
    if ((e as NodeJS.ErrnoException).code === "ENOENT") return [];
    throw e;
  }
  const out: T[] = [];
  for (const line of text.split("\n")) {
    if (!line.trim()) continue;
    try {
      out.push(JSON.parse(line) as T);
    } catch {
      /* a torn line from a crash mid-write */
    }
  }
  return out;
}

export function replaceJson(file: string, value: unknown): void {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const tmp = `${file}.${process.pid}.tmp`;
  const fd = fs.openSync(tmp, "w");
  try {
    fs.writeSync(fd, JSON.stringify(value, null, 2));
    fs.fsyncSync(fd);
  } finally {
    fs.closeSync(fd);
  }
  fs.renameSync(tmp, file);
}

export function readJson<T>(file: string, fallback: T): T {
  try {
    return JSON.parse(fs.readFileSync(file, "utf8")) as T;
  } catch (e) {
    if ((e as NodeJS.ErrnoException).code === "ENOENT") return fallback;
    throw e;
  }
}
```

- [ ] **Step 5: Run it and watch it pass**

Run: `cd harness && npm run bench:test`
Expected: `# pass 4`, `# fail 0`.

- [ ] **Step 6: Commit**

```bash
git add harness/bench harness/package.json harness/package-lock.json
git commit -m "Add harness-bench package with the append-only folder log"
```

---

### Task 2: The single-writer lock

**Files:**
- Create: `harness/bench/src/lock.ts`, `harness/bench/test/lock.test.ts`

**Interfaces:**
```ts
export type Lock = { release(): void };
export class FolderLocked extends Error { holder: string }
export function takeLock(dir: string, opts?: { wait?: boolean; onWaiting?: (holder: string) => void }): Promise<Lock>;
```

- [ ] **Step 1: Write the failing test**

`harness/bench/test/lock.test.ts`:
```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { takeLock, FolderLocked } from "../src/lock.ts";

test("a second instance on the same folder refuses and names the holder", async () => {
  const d = fs.mkdtempSync(path.join(os.tmpdir(), "bench-lock-"));
  const a = await takeLock(d);
  await assert.rejects(takeLock(d), (e: unknown) => e instanceof FolderLocked && e.holder.includes(`pid ${process.pid}`));
  a.release();
  const b = await takeLock(d);
  b.release();
});

test("wait mode takes the lock once the holder lets go", async () => {
  const d = fs.mkdtempSync(path.join(os.tmpdir(), "bench-lock-"));
  const a = await takeLock(d);
  let waited = "";
  const pending = takeLock(d, { wait: true, onWaiting: (h) => (waited = h) });
  await new Promise((r) => setTimeout(r, 300));
  a.release();
  const b = await pending;
  assert.match(waited, /pid \d+/);
  b.release();
});
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cd harness && node --test bench/test/lock.test.ts`
Expected: FAIL with `Cannot find module '.../bench/src/lock.ts'`.

- [ ] **Step 3: Implement**

`harness/bench/src/lock.ts`:
```ts
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

/**
 * The folder-level fence: one harness-bench writes a bench folder. Node has no
 * flock, so flock(1) holds it for us — a child that prints "locked" once it has
 * the lock and then blocks on our stdin, so the lock dies with this process
 * (pipe closes → cat exits → fd closes) and never outlives it. On NFS 4.1 the
 * lock is a lease; a dead node's lock goes when the lease expires.
 */
export class FolderLocked extends Error {
  holder: string;
  constructor(holder: string) {
    super(`bench folder is locked by ${holder}`);
    this.holder = holder;
  }
}
export type Lock = { release(): void };

export function takeLock(dir: string, opts: { wait?: boolean; onWaiting?: (holder: string) => void } = {}): Promise<Lock> {
  const file = path.join(dir, ".lock");
  const holderFile = path.join(dir, ".lock.holder");
  fs.mkdirSync(dir, { recursive: true });
  const holder = () => {
    try { return fs.readFileSync(holderFile, "utf8").trim(); } catch { return "an unknown holder"; }
  };
  return new Promise((resolve, reject) => {
    const child = spawn("flock", ["-x", ...(opts.wait ? [] : ["-n"]), file, "-c", "echo locked; exec cat"], { stdio: ["pipe", "pipe", "inherit"] });
    let got = false;
    const waiting = opts.wait ? setTimeout(() => opts.onWaiting?.(holder()), 100) : undefined;
    child.stdout.on("data", (d: Buffer) => {
      if (got || !d.toString().includes("locked")) return;
      got = true;
      clearTimeout(waiting);
      fs.writeFileSync(holderFile, `${process.env.NODE_NAME ?? os.hostname()} pid ${process.pid}`);
      resolve({ release: () => child.stdin.end() });
    });
    child.on("error", (e) => reject(new Error(`flock(1) is required: ${e.message}`)));
    child.on("exit", (code) => {
      clearTimeout(waiting);
      if (!got) reject(code === 1 ? new FolderLocked(holder()) : new Error(`flock exited ${code}`));
    });
  });
}
```

- [ ] **Step 4: Run it and watch it pass**

Run: `cd harness && node --test bench/test/lock.test.ts`
Expected: `# pass 2`, `# fail 0`.

- [ ] **Step 5: Commit**

```bash
git add harness/bench/src/lock.ts harness/bench/test/lock.test.ts
git commit -m "Fence a bench folder to one writer with flock"
```

---

### Task 3: The session list

**Files:**
- Create: `harness/bench/src/sessions.ts`, `harness/bench/test/sessions.test.ts`

**Interfaces:**
```ts
export type SessionRow = { id: string; name: string; seq: number; file?: string; created: number; lastActive: number; archived: boolean; model?: string };
export class SessionList {
  constructor(dir: string);                     // reads /bench/sessions.json
  all(): SessionRow[];
  get(id: string): SessionRow | undefined;
  create(model?: string): SessionRow;           // s-(max seq + 1), name "session N"
  update(id: string, patch: Partial<SessionRow>): SessionRow;   // throws "no session <id>"
  remove(id: string): void;
  merge(rows: SessionRow[]): string[];          // import: adds unknown ids only, returns added ids
}
```

- [ ] **Step 1: Write the failing test**

`harness/bench/test/sessions.test.ts`:
```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { SessionList } from "../src/sessions.ts";

const dir = () => fs.mkdtempSync(path.join(os.tmpdir(), "bench-sessions-"));

test("create numbers after the highest seq and persists", () => {
  const d = dir();
  const a = new SessionList(d);
  const s1 = a.create();
  const s2 = a.create();
  assert.equal(s1.id, "s-1");
  assert.equal(s2.name, "session 2");
  a.update("s-1", { archived: true, file: "/bench/sessions/x.jsonl" });
  const b = new SessionList(d);
  assert.deepEqual(b.all().map((s) => [s.id, s.archived]), [["s-1", true], ["s-2", false]]);
});

test("merge adds only unknown ids, so a re-run is a no-op", () => {
  const d = dir();
  const a = new SessionList(d);
  const row = { id: "bench", name: "session 1", seq: 1, created: 1, lastActive: 1, archived: false };
  assert.deepEqual(a.merge([row]), ["bench"]);
  assert.deepEqual(a.merge([{ ...row, name: "changed" }]), []);
  assert.equal(new SessionList(d).get("bench")!.name, "session 1");
  assert.equal(a.create().id, "s-2");
});

test("remove drops the row", () => {
  const a = new SessionList(dir());
  a.create();
  a.remove("s-1");
  assert.equal(a.all().length, 0);
  assert.throws(() => a.update("s-1", {}), /no session s-1/);
});
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cd harness && node --test bench/test/sessions.test.ts`
Expected: FAIL with `Cannot find module '.../bench/src/sessions.ts'`.

- [ ] **Step 3: Implement**

`harness/bench/src/sessions.ts`:
```ts
import path from "node:path";
import { readJson, replaceJson } from "./log.ts";

export type SessionRow = { id: string; name: string; seq: number; file?: string; created: number; lastActive: number; archived: boolean; model?: string };

/** `/bench/sessions.json`: the list is small, so every change replaces it whole. */
export class SessionList {
  private file: string;
  private rows: SessionRow[];
  constructor(dir: string) {
    this.file = path.join(dir, "sessions.json");
    this.rows = readJson<SessionRow[]>(this.file, []);
  }
  private save() {
    replaceJson(this.file, this.rows);
  }
  all(): SessionRow[] {
    return this.rows.map((r) => ({ ...r }));
  }
  get(id: string): SessionRow | undefined {
    return this.rows.find((r) => r.id === id);
  }
  create(model?: string): SessionRow {
    const seq = Math.max(0, ...this.rows.map((r) => r.seq)) + 1;
    const now = Date.now();
    const row: SessionRow = { id: `s-${seq}`, name: `session ${seq}`, seq, created: now, lastActive: now, archived: false, model };
    this.rows.push(row);
    this.save();
    return { ...row };
  }
  update(id: string, patch: Partial<SessionRow>): SessionRow {
    const r = this.get(id);
    if (!r) throw new Error(`no session ${id}`);
    Object.assign(r, patch, { id: r.id });
    this.save();
    return { ...r };
  }
  remove(id: string): void {
    this.rows = this.rows.filter((r) => r.id !== id);
    this.save();
  }
  merge(rows: SessionRow[]): string[] {
    const added = rows.filter((r) => !this.get(r.id));
    if (!added.length) return [];
    this.rows.push(...added.map((r) => ({ ...r })));
    this.save();
    return added.map((r) => r.id);
  }
}
```

- [ ] **Step 4: Run it and watch it pass**

Run: `cd harness && node --test bench/test/sessions.test.ts`
Expected: `# pass 3`, `# fail 0`.

- [ ] **Step 5: Commit**

```bash
git add harness/bench/src/sessions.ts harness/bench/test/sessions.test.ts
git commit -m "Keep the bench session list in sessions.json"
```

---

### Task 4: The exchange log and its two views

**Files:**
- Create: `harness/bench/src/exchanges.ts`, `harness/bench/test/exchanges.test.ts`

**Interfaces:**
```ts
export type Exchange = { ts: number; id: string; session: string; workspace: string; dir: "in" | "out"; text: string; state: string; ref?: string };
type Line = Exchange | { ts: number; id: string; state: string } | { ts: number; session: string; discarded: true };
export class ExchangeLog {
  constructor(dir: string);                                   // folds exchanges.jsonl once
  record(e: Omit<Exchange, "ts">): Exchange;                  // appends, updates both views
  transition(id: string, state: string): void;
  discard(session: string): void;
  bySession(session: string, after?: number): Exchange[];     // after = ts, exclusive
  byWorkspace(workspace: string, after?: number): Exchange[];
  recent(n: number): Exchange[];
}
```

- [ ] **Step 1: Write the failing test**

`harness/bench/test/exchanges.test.ts`:
```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { ExchangeLog } from "../src/exchanges.ts";

const dir = () => fs.mkdtempSync(path.join(os.tmpdir(), "bench-ex-"));

test("views rebuilt from the log agree with the live ones", () => {
  const d = dir();
  const a = new ExchangeLog(d);
  a.record({ id: "e1", session: "s-1", workspace: "api", dir: "out", text: "run tests", state: "sent" });
  a.record({ id: "e2", session: "s-2", workspace: "api", dir: "out", text: "bump dep", state: "sent" });
  a.transition("e1", "done");
  const b = new ExchangeLog(d);
  assert.deepEqual(b.bySession("s-1"), a.bySession("s-1"));
  assert.deepEqual(b.byWorkspace("api"), a.byWorkspace("api"));
  assert.equal(b.bySession("s-1")[0].state, "done");
  // Both views are filters over one log, so every row is in both.
  const ws = b.byWorkspace("api").map((e) => e.id).sort();
  const ss = [...b.bySession("s-1"), ...b.bySession("s-2")].map((e) => e.id).sort();
  assert.deepEqual(ws, ss);
});

test("a discard line removes the session's rows from the workspace view, also after a restart", () => {
  const d = dir();
  const a = new ExchangeLog(d);
  a.record({ id: "e1", session: "s-1", workspace: "api", dir: "out", text: "x", state: "sent" });
  a.record({ id: "e2", session: "s-2", workspace: "api", dir: "out", text: "y", state: "sent" });
  a.discard("s-1");
  assert.deepEqual(a.byWorkspace("api").map((e) => e.id), ["e2"]);
  assert.deepEqual(new ExchangeLog(d).byWorkspace("api").map((e) => e.id), ["e2"]);
  assert.deepEqual(new ExchangeLog(d).bySession("s-1"), []);
});

test("after pages by timestamp", async () => {
  const a = new ExchangeLog(dir());
  const first = a.record({ id: "e1", session: "s-1", workspace: "api", dir: "out", text: "x", state: "sent" });
  await new Promise((r) => setTimeout(r, 2));
  a.record({ id: "e2", session: "s-1", workspace: "api", dir: "in", text: "ok", state: "done" });
  assert.deepEqual(a.bySession("s-1", first.ts).map((e) => e.id), ["e2"]);
});
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cd harness && node --test bench/test/exchanges.test.ts`
Expected: FAIL with `Cannot find module '.../bench/src/exchanges.ts'`.

- [ ] **Step 3: Implement**

`harness/bench/src/exchanges.ts`:
```ts
import path from "node:path";
import { appendLine, readLines } from "./log.ts";

export type Exchange = { ts: number; id: string; session: string; workspace: string; dir: "in" | "out"; text: string; state: string; ref?: string };
type Line = Partial<Exchange> & { ts: number; discarded?: true };

/**
 * `/bench/exchanges.jsonl`: every message between a session and a workspace,
 * one log. A session's queue and a workspace's queue are two filters over the
 * same rows, so they cannot disagree.
 */
export class ExchangeLog {
  private file: string;
  private rows = new Map<string, Exchange>();
  constructor(dir: string) {
    this.file = path.join(dir, "exchanges.jsonl");
    for (const l of readLines<Line>(this.file)) this.fold(l);
  }
  private fold(l: Line) {
    if (l.discarded && l.session) {
      for (const [id, e] of this.rows) if (e.session === l.session) this.rows.delete(id);
    } else if (l.session && l.workspace && l.id) {
      this.rows.set(l.id, l as Exchange);
    } else if (l.id && l.state) {
      const e = this.rows.get(l.id);
      if (e) this.rows.set(l.id, { ...e, state: l.state });
    }
  }
  private write(l: Line) {
    appendLine(this.file, l);
    this.fold(l);
  }
  record(e: Omit<Exchange, "ts">): Exchange {
    const row = { ts: Date.now(), ...e };
    this.write(row);
    return row;
  }
  transition(id: string, state: string): void {
    this.write({ ts: Date.now(), id, state });
  }
  discard(session: string): void {
    this.write({ ts: Date.now(), session, discarded: true });
  }
  private where(pred: (e: Exchange) => boolean, after = 0): Exchange[] {
    return [...this.rows.values()].filter((e) => pred(e) && e.ts > after).sort((a, b) => a.ts - b.ts);
  }
  bySession(session: string, after?: number): Exchange[] {
    return this.where((e) => e.session === session, after);
  }
  byWorkspace(workspace: string, after?: number): Exchange[] {
    return this.where((e) => e.workspace === workspace, after);
  }
  recent(n: number): Exchange[] {
    return this.where(() => true).slice(-n);
  }
}
```

- [ ] **Step 4: Run it and watch it pass**

Run: `cd harness && node --test bench/test/exchanges.test.ts`
Expected: `# pass 3`, `# fail 0`.

- [ ] **Step 5: Commit**

```bash
git add harness/bench/src/exchanges.ts harness/bench/test/exchanges.test.ts
git commit -m "Record session and workspace exchanges in one log with two views"
```

---

### Task 5: Tasks and processes, with lost marking

**Files:**
- Create: `harness/bench/src/ledger.ts`, `harness/bench/test/ledger.test.ts`

**Interfaces:**
```ts
export type TaskRow = { id: string; session: string; n?: number; tool: string; arg: string; state: "running" | "background" | "done" | "failed" | "cancelled" | "lost"; started: number; ended?: number };
export class Tasks {
  constructor(dir: string);
  transition(t: Partial<TaskRow> & { id: string }): TaskRow;  // appends to tasks.jsonl, folds
  all(): TaskRow[];
  markLost(): TaskRow[];                                       // running|background → lost; returns them
}
export type ProcRow = { id: string; session: string; name: string; command: string; pid?: number; started: number; ended?: number; code?: number | null; lost?: true };
export class Procs {
  constructor(dir: string, alive?: (pid: number) => boolean);
  snapshot(session: string, rows: Omit<ProcRow, "session">[]): void;  // replaces this session's rows in procs.json
  all(): ProcRow[];
  markLost(): ProcRow[];                                       // rows without ended whose pid is not alive
}
```

- [ ] **Step 1: Write the failing test**

`harness/bench/test/ledger.test.ts`:
```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Tasks, Procs } from "../src/ledger.ts";

const dir = () => fs.mkdtempSync(path.join(os.tmpdir(), "bench-ledger-"));

test("tasks fold from the log and in-flight ones are lost after a restart", () => {
  const d = dir();
  const a = new Tasks(d);
  a.transition({ id: "t1", session: "s-1", tool: "Bash", arg: "make", state: "running", started: 1 });
  a.transition({ id: "t2", session: "s-1", tool: "Bash", arg: "ls", state: "running", started: 2 });
  a.transition({ id: "t1", n: 1, state: "background" });
  a.transition({ id: "t2", state: "done", ended: 3 });
  const b = new Tasks(d);
  assert.deepEqual(b.markLost().map((t) => t.id), ["t1"]);
  assert.equal(new Tasks(d).all().find((t) => t.id === "t1")!.state, "lost");
  assert.equal(new Tasks(d).markLost().length, 0);
});

test("procs keep other sessions' rows, and dead pids are lost", () => {
  const d = dir();
  const p = new Procs(d, (pid) => pid === 100);
  p.snapshot("s-1", [{ id: "p1", name: "vite", command: "npm run dev", pid: 100, started: 1 }]);
  p.snapshot("s-2", [{ id: "p1", name: "tunnel", command: "kl tunnel", pid: 200, started: 1 }]);
  p.snapshot("s-1", [{ id: "p1", name: "vite", command: "npm run dev", pid: 100, started: 1 }, { id: "p2", name: "w", command: "w", pid: 300, started: 2, ended: 5, code: 0 }]);
  assert.equal(p.all().length, 3);
  const lost = new Procs(d, (pid) => pid === 100).markLost();
  assert.deepEqual(lost.map((r) => `${r.session}/${r.id}`), ["s-2/p1"]);
  const row = new Procs(d).all().find((r) => r.session === "s-2")!;
  assert.equal(row.lost, true);
  assert.equal(typeof row.ended, "number");
});
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cd harness && node --test bench/test/ledger.test.ts`
Expected: FAIL with `Cannot find module '.../bench/src/ledger.ts'`.

- [ ] **Step 3: Implement**

`harness/bench/src/ledger.ts`:
```ts
import path from "node:path";
import { appendLine, readJson, readLines, replaceJson } from "./log.ts";

export type TaskRow = { id: string; session: string; n?: number; tool: string; arg: string; state: "running" | "background" | "done" | "failed" | "cancelled" | "lost"; started: number; ended?: number };

/** `/bench/tasks.jsonl`: one line per transition; the current table is the fold. */
export class Tasks {
  private file: string;
  private rows = new Map<string, TaskRow>();
  constructor(dir: string) {
    this.file = path.join(dir, "tasks.jsonl");
    for (const l of readLines<Partial<TaskRow> & { id: string }>(this.file)) this.fold(l);
  }
  private fold(l: Partial<TaskRow> & { id: string }): TaskRow {
    const row = { ...(this.rows.get(l.id) ?? {}), ...l } as TaskRow;
    this.rows.set(l.id, row);
    return row;
  }
  transition(t: Partial<TaskRow> & { id: string }): TaskRow {
    appendLine(this.file, t);
    return this.fold(t);
  }
  all(): TaskRow[] {
    return [...this.rows.values()];
  }
  /** A new process holds none of the old one's commands: whatever was in flight is gone. */
  markLost(): TaskRow[] {
    const now = Date.now();
    return this.all().filter((t) => t.state === "running" || t.state === "background").map((t) => this.transition({ id: t.id, state: "lost", ended: now }));
  }
}

export type ProcRow = { id: string; session: string; name: string; command: string; pid?: number; started: number; ended?: number; code?: number | null; lost?: true };

const pidAlive = (pid: number) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch (e) {
    return (e as NodeJS.ErrnoException).code === "EPERM";
  }
};

/** `/bench/procs.json`: the live table, replaced whole; each session publishes only its own rows. */
export class Procs {
  private file: string;
  private rows: ProcRow[];
  private alive: (pid: number) => boolean;
  constructor(dir: string, alive: (pid: number) => boolean = pidAlive) {
    this.file = path.join(dir, "procs.json");
    this.rows = readJson<ProcRow[]>(this.file, []);
    this.alive = alive;
  }
  snapshot(session: string, rows: Omit<ProcRow, "session">[]): void {
    this.rows = [...this.rows.filter((r) => r.session !== session), ...rows.map((r) => ({ ...r, session }))];
    replaceJson(this.file, this.rows);
  }
  all(): ProcRow[] {
    return this.rows.map((r) => ({ ...r }));
  }
  markLost(): ProcRow[] {
    const now = Date.now();
    const lost = this.rows.filter((r) => r.ended === undefined && !(r.pid && this.alive(r.pid)));
    for (const r of lost) Object.assign(r, { ended: now, lost: true as const });
    if (lost.length) replaceJson(this.file, this.rows);
    return lost.map((r) => ({ ...r }));
  }
}
```

- [ ] **Step 4: Run it and watch it pass**

Run: `cd harness && node --test bench/test/ledger.test.ts`
Expected: `# pass 2`, `# fail 0`.

- [ ] **Step 5: Commit**

```bash
git add harness/bench/src/ledger.ts harness/bench/test/ledger.test.ts
git commit -m "Keep bench tasks and processes, marking in-flight ones lost on restart"
```

---

### Task 6: The pi RPC child and a fake pi

**Files:**
- Create: `harness/bench/src/rpc-child.ts`, `harness/bench/test/fake-pi.ts`, `harness/bench/test/rpc-child.test.ts`

**Interfaces:**
```ts
export type PiEvent = Record<string, unknown> & { type: string; id?: string };
export const READ_ONLY_TOOLS: string;
export type ChildOpts = { dir: string; file?: string; fork?: string; model: string; bin?: string; extDir?: string; cwd?: string };
export class RpcChild {
  constructor(id: string, opts: ChildOpts, onEvent: (ev: PiEvent) => void);
  start(): void;
  stop(): void;
  running(): boolean;
  send(cmd: Record<string, unknown>): Promise<PiEvent>;   // rejects "pi is not running" after exit
}
```

The fake pi answers `get_state` with `sessionFile`, `get_messages` with what it was prompted, and `prompt` with `agent_start`, one `message_update` text delta, and `agent_end`. It writes the files it names, like pi does. It also emits one `setWidget` for `harness:exchange` when the prompt text is `exchange`, and exits on `crash`.

- [ ] **Step 1: Write the fake pi**

`harness/bench/test/fake-pi.ts`:
```ts
#!/usr/bin/env node
// A pi stand-in speaking just enough of `pi --mode rpc` for harness-bench's tests.
import fs from "node:fs";
import path from "node:path";

const argv = process.argv.slice(2);
const flag = (n: string) => (argv.includes(n) ? argv[argv.indexOf(n) + 1] : undefined);
const dir = flag("--session-dir") ?? ".";
const file = flag("--session") ?? path.join(dir, `fake-${process.pid}-${Date.now()}.jsonl`);
if (!fs.existsSync(file)) fs.writeFileSync(file, JSON.stringify({ type: "session", version: 3, id: path.basename(file, ".jsonl"), timestamp: new Date().toISOString(), cwd: process.cwd() }) + "\n");
const messages: unknown[] = [];
const out = (v: unknown) => process.stdout.write(JSON.stringify(v) + "\n");
let buf = "";
process.stdin.on("data", (d) => {
  buf += d.toString();
  let at;
  while ((at = buf.indexOf("\n")) >= 0) {
    const cmd = JSON.parse(buf.slice(0, at));
    buf = buf.slice(at + 1);
    const ok = (data?: unknown) => out({ type: "response", id: cmd.id, command: cmd.type, success: true, data });
    if (cmd.type === "get_state") ok({ sessionFile: file, isStreaming: false });
    else if (cmd.type === "get_messages") ok({ messages });
    else if (cmd.type === "abort") ok();
    else if (cmd.type === "prompt") {
      if (cmd.message === "crash") process.exit(3);
      ok();
      messages.push({ role: "user", content: cmd.message, timestamp: Date.now() });
      out({ type: "agent_start" });
      if (cmd.message === "exchange") out({ type: "extension_ui_request", id: "w1", method: "setWidget", widgetKey: "harness:exchange", widgetLines: [JSON.stringify({ id: "e1", workspace: "api", dir: "out", text: "kl_workspace_start api", state: "sent" })] });
      out({ type: "message_update", assistantMessageEvent: { type: "text_delta", delta: `echo ${cmd.message}` } });
      messages.push({ role: "assistant", content: [{ type: "text", text: `echo ${cmd.message}` }], timestamp: Date.now() });
      out({ type: "agent_end" });
    } else out({ type: "response", id: cmd.id, command: cmd.type, success: false, error: `fake pi: ${cmd.type}` });
  }
});
```

- [ ] **Step 2: Write the failing test**

`harness/bench/test/rpc-child.test.ts`:
```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { RpcChild, type PiEvent } from "../src/rpc-child.ts";

export const FAKE = path.join(path.dirname(fileURLToPath(import.meta.url)), "fake-pi.ts");

test("a command resolves on its response; events stream in order", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-rpc-"));
  const seen: PiEvent[] = [];
  const c = new RpcChild("s-1", { dir, model: "fake/model", bin: FAKE }, (ev) => seen.push(ev));
  c.start();
  const st = await c.send({ type: "get_state" });
  assert.match((st.data as { sessionFile: string }).sessionFile, /\.jsonl$/);
  await c.send({ type: "prompt", message: "hi" });
  await new Promise((r) => setTimeout(r, 100));
  assert.deepEqual(seen.filter((e) => e.type !== "response" && e.type !== "started").map((e) => e.type), ["agent_start", "message_update", "agent_end"]);
  c.stop();
});

test("a dead child rejects waiting and later sends", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-rpc-"));
  const seen: PiEvent[] = [];
  const c = new RpcChild("s-1", { dir, model: "m", bin: FAKE }, (ev) => seen.push(ev));
  c.start();
  await assert.rejects(c.send({ type: "prompt", message: "crash" }), /pi exited \(3\)/);
  assert.equal(c.running(), false);
  assert.ok(seen.some((e) => e.type === "exit" && e.code === 3));
});
```

- [ ] **Step 3: Run it and watch it fail**

Run: `cd harness && node --test bench/test/rpc-child.test.ts`
Expected: FAIL with `Cannot find module '.../bench/src/rpc-child.ts'`.

- [ ] **Step 4: Implement**

`harness/bench/src/rpc-child.ts`, which is `src/pi.ts` without Electron, memo files or ssh. The folder is the memory now:
```ts
import { spawn, type ChildProcess } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

/**
 * One session's pi, in RPC mode: JSONL over stdio. Framing is strict LF; Node's
 * readline also splits on U+2028/2029, which are legal inside JSON strings.
 * The session file lives in the bench folder (`--session-dir`), so nothing
 * here remembers anything — reopening a session is `--session <file>`.
 */
export type PiEvent = Record<string, unknown> & { type: string; id?: string };
export const READ_ONLY_TOOLS = "read,grep,find,ls,kl_workspaces,kl_workspace,kl_environments,kl_environment,kl_regions,kl_quota,kl_volumes,kl_builder,kl_whoami";
const HARNESS = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");

export type ChildOpts = { dir: string; file?: string; fork?: string; model: string; bin?: string; extDir?: string; cwd?: string };

export class RpcChild {
  readonly id: string;
  private opts: ChildOpts;
  private onEvent: (ev: PiEvent) => void;
  private child?: ChildProcess;
  private buf = "";
  private seq = 0;
  private waiting = new Map<string, { resolve: (r: PiEvent) => void; reject: (e: Error) => void }>();

  constructor(id: string, opts: ChildOpts, onEvent: (ev: PiEvent) => void) {
    this.id = id;
    this.opts = opts;
    this.onEvent = onEvent;
  }

  running(): boolean {
    return !!this.child;
  }

  start(): void {
    if (this.child) return;
    const o = this.opts;
    const bin = o.bin ?? process.env.HARNESS_PI_BIN ?? path.join(HARNESS, "node_modules", ".bin", "pi");
    const extDir = o.extDir ?? path.join(HARNESS, "pi");
    const exts = o.fork ? [] : ["background.ts", "process.ts", "kloudlite.ts"].flatMap((f) => ["-e", path.join(extDir, f)]);
    const args = ["--mode", "rpc", "--model", o.model, "--session-dir", o.dir, ...exts, ...(o.file ? ["--session", o.file] : []), ...(o.fork ? ["--fork", o.fork, "--tools", READ_ONLY_TOOLS] : [])];
    const child = spawn(bin, args, { stdio: ["pipe", "pipe", "pipe"], env: process.env, cwd: o.cwd ?? process.env.HOME });
    this.child = child;
    child.stdout!.on("data", (d: Buffer) => this.feed(d.toString("utf8")));
    let errTail = "";
    child.stderr!.on("data", (d: Buffer) => {
      const t = d.toString("utf8");
      errTail = (errTail + t).slice(-2000);
      this.onEvent({ type: "stderr", text: t });
    });
    child.on("exit", (code) => {
      this.child = undefined;
      const err = new Error(`pi exited (${code})`);
      for (const w of this.waiting.values()) w.reject(err);
      this.waiting.clear();
      this.onEvent({ type: "exit", code, stderr: errTail.trim().split("\n").filter((l) => l.trim()).slice(-3).join(" · ") });
    });
    this.onEvent({ type: "started", host: "bench", model: o.model, resumed: !!o.file, forked: !!o.fork });
  }

  stop(): void {
    this.child?.kill();
  }

  send(cmd: Record<string, unknown>): Promise<PiEvent> {
    const c = this.child;
    if (!c) return Promise.reject(new Error("pi is not running"));
    const id = `c${++this.seq}`;
    return new Promise((resolve, reject) => {
      this.waiting.set(id, { resolve, reject });
      c.stdin!.write(JSON.stringify({ ...cmd, id }) + "\n");
    });
  }

  private feed(chunk: string) {
    this.buf += chunk;
    let at: number;
    while ((at = this.buf.indexOf("\n")) >= 0) {
      const line = this.buf.slice(0, at).replace(/\r$/, "");
      this.buf = this.buf.slice(at + 1);
      if (!line) continue;
      let ev: PiEvent;
      try {
        ev = JSON.parse(line) as PiEvent;
      } catch {
        this.onEvent({ type: "stderr", text: line });
        continue;
      }
      const w = ev.type === "response" && ev.id ? this.waiting.get(ev.id) : undefined;
      if (w) {
        this.waiting.delete(ev.id!);
        w.resolve(ev);
      }
      this.onEvent(ev);
    }
  }
}
```

- [ ] **Step 5: Run it and watch it pass**

Run: `cd harness && chmod +x bench/test/fake-pi.ts && node --test bench/test/rpc-child.test.ts`
Expected: `# pass 2`, `# fail 0`.

- [ ] **Step 6: Commit**

```bash
git add harness/bench/src/rpc-child.ts harness/bench/test/fake-pi.ts harness/bench/test/rpc-child.test.ts
git commit -m "Run a bench session's pi as an RPC child over the bench folder"
```

---

### Task 7: Read-only transcripts through the pi SDK

**Files:**
- Create: `harness/bench/src/reader.ts`, `harness/bench/test/reader.test.ts`

**Interfaces:**
```ts
export function transcript(file: string): unknown[];     // SessionManager.open(file).buildSessionContext().messages
export function page<T>(all: T[], after?: number, limit?: number): { messages: T[]; total: number };
export function listFiles(dir: string): Promise<{ path: string; id: string; name?: string; modified: number; messageCount: number }[]>;
```

- [ ] **Step 1: Write the failing test**

`harness/bench/test/reader.test.ts`:
```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { transcript, page, listFiles } from "../src/reader.ts";

// A pi session file written by hand in pi's own format (SessionHeader +
// SessionMessageEntry, session-manager.d.ts), so the test needs no model.
function sessionFile(dir: string, cwd: string) {
  const f = path.join(dir, "2026-09-13T00-00-00_abc.jsonl");
  const t = new Date().toISOString();
  const lines = [
    { type: "session", version: 3, id: "abc", timestamp: t, cwd },
    { type: "message", id: "m1", parentId: null, timestamp: t, message: { role: "user", content: "hello", timestamp: 1 } },
    { type: "message", id: "m2", parentId: "m1", timestamp: t, message: { role: "assistant", content: [{ type: "text", text: "hi" }], api: "x", provider: "x", model: "x", usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } }, stopReason: "stop", timestamp: 2 } },
  ];
  fs.writeFileSync(f, lines.map((l) => JSON.stringify(l)).join("\n") + "\n");
  return f;
}

test("a stopped session's transcript is read without starting an agent", () => {
  const d = fs.mkdtempSync(path.join(os.tmpdir(), "bench-read-"));
  const ms = transcript(sessionFile(d, "/home/kl")) as { role: string }[];
  assert.deepEqual(ms.map((m) => m.role), ["user", "assistant"]);
});

test("page takes after as an index and limit as a count", () => {
  assert.deepEqual(page([1, 2, 3, 4], 1, 2), { messages: [2, 3], total: 4 });
  assert.deepEqual(page([1, 2], 5), { messages: [], total: 2 });
});

test("listFiles lists the folder's sessions", async () => {
  const d = fs.mkdtempSync(path.join(os.tmpdir(), "bench-read-"));
  sessionFile(d, "/home/kl");
  const rows = await listFiles(d);
  assert.equal(rows.length, 1);
  assert.equal(rows[0].messageCount, 2);
});
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cd harness && node --test bench/test/reader.test.ts`
Expected: FAIL with `Cannot find module '.../bench/src/reader.ts'`.

- [ ] **Step 3: Implement**

`harness/bench/src/reader.ts`:
```ts
import { SessionManager } from "@mariozechner/pi-coding-agent";

/**
 * History without an agent: pi's SDK reads its own JSONL (tree, compaction,
 * branch summaries resolved) and hands back the messages the model would see.
 * This is all `--read-only` needs, and what serves an archived session.
 */
export function transcript(file: string): unknown[] {
  return SessionManager.open(file).buildSessionContext().messages;
}

export function page<T>(all: T[], after = 0, limit = all.length): { messages: T[]; total: number } {
  return { messages: all.slice(after, after + limit), total: all.length };
}

export async function listFiles(dir: string) {
  // pi's list filters by cwd; bench sessions all start in the home.
  const rows = await SessionManager.listAll().catch(() => []);
  const here = rows.length ? rows : [];
  const inDir = await SessionManager.list(process.env.HOME ?? "/home/kl", dir);
  const all = [...inDir, ...here.filter((r) => r.path.startsWith(dir) && !inDir.some((x) => x.path === r.path))];
  return all.map((r) => ({ path: r.path, id: r.id, name: r.name, modified: r.modified.getTime(), messageCount: r.messageCount }));
}
```

- [ ] **Step 4: Run it and check the list half**

Run: `cd harness && node --test bench/test/reader.test.ts`
Expected: `# pass 3`.

If `listFiles` returns 0, `SessionManager.list` filtered the file out by `cwd`. Read `dist/core/session-manager.js` (`list`) to see how it filters, and replace the body with the one call it supports: `SessionManager.list(<the header's cwd>, dir)`. Do not guess.

- [ ] **Step 5: Commit**

```bash
git add harness/bench/src/reader.ts harness/bench/test/reader.test.ts
git commit -m "Read bench transcripts through the pi SDK without an agent"
```

---

### Task 8: The writability guard

**Files:**
- Create: `harness/bench/src/guard.ts`, `harness/bench/test/guard.test.ts`

**Interfaces:**
```ts
export class Writable {
  constructor(dir: string, onChange: (ok: boolean, reason?: string) => void);
  ok(): boolean;
  reason(): string | undefined;
  run<T>(write: () => T): T;        // a throw flips to not writable, then rethrows
  probe(): boolean;                 // writes .health (appendLine); success restores
}
```

- [ ] **Step 1: Write the failing test**

`harness/bench/test/guard.test.ts`:
```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Writable } from "../src/guard.ts";

test("a failed write flips the guard and a good probe restores it", () => {
  const d = fs.mkdtempSync(path.join(os.tmpdir(), "bench-guard-"));
  const changes: boolean[] = [];
  const w = new Writable(d, (ok) => changes.push(ok));
  assert.throws(() => w.run(() => { throw Object.assign(new Error("EIO: i/o error"), { code: "EIO" }); }), /EIO/);
  assert.equal(w.ok(), false);
  assert.match(w.reason()!, /EIO/);
  fs.chmodSync(d, 0o500);
  assert.equal(w.probe(), false);
  fs.chmodSync(d, 0o700);
  assert.equal(w.probe(), true);
  assert.deepEqual(changes, [false, true]);
});
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cd harness && node --test bench/test/guard.test.ts`
Expected: FAIL with `Cannot find module '.../bench/src/guard.ts'`.

- [ ] **Step 3: Implement**

`harness/bench/src/guard.ts`:
```ts
import path from "node:path";
import { appendLine } from "./log.ts";

/**
 * A bench whose folder stops taking writes must stop taking prompts: work kept
 * only in memory is lost on the next reschedule, silently. Any failed write
 * flips this; the probe beat (main.ts, every 10 s) flips it back.
 */
export class Writable {
  private file: string;
  private onChange: (ok: boolean, reason?: string) => void;
  private why?: string;
  constructor(dir: string, onChange: (ok: boolean, reason?: string) => void) {
    this.file = path.join(dir, ".health");
    this.onChange = onChange;
  }
  ok(): boolean {
    return this.why === undefined;
  }
  reason(): string | undefined {
    return this.why;
  }
  private set(why: string | undefined) {
    const was = this.ok();
    this.why = why;
    if (was !== this.ok()) this.onChange(this.ok(), why);
  }
  run<T>(write: () => T): T {
    try {
      return write();
    } catch (e) {
      this.set((e as Error).message);
      throw e;
    }
  }
  probe(): boolean {
    try {
      appendLine(this.file, { ts: Date.now() });
      this.set(undefined);
      return true;
    } catch (e) {
      this.set((e as Error).message);
      return false;
    }
  }
}
```

`.health` grows by one short line every 10 s, which is about 1.5 MB a year. `main.ts` truncates it on start.

- [ ] **Step 4: Run it and watch it pass**

Run: `cd harness && node --test bench/test/guard.test.ts`
Expected: `# pass 1`. When run as root, the chmod has no effect and the second probe assertion fails. Run the tests as a normal user, which is uid 1000 in the pod.

- [ ] **Step 5: Commit**

```bash
git add harness/bench/src/guard.ts harness/bench/test/guard.test.ts
git commit -m "Refuse prompts while the bench folder cannot be written"
```

---

### Task 9: The Bench core — sessions, children, event folding, delete, archive

**Files:**
- Create: `harness/bench/src/bench.ts`, `harness/bench/test/bench.test.ts`

**Interfaces:**
```ts
export type BenchEvent = { type: string; [k: string]: unknown };
export type BenchOpts = { dir: string; readOnly: boolean; model: string; bin?: string; extDir?: string };
export class Bench {
  constructor(opts: BenchOpts);
  readonly sessions: SessionList; readonly exchanges: ExchangeLog; readonly tasks: Tasks; readonly procs: Procs; readonly writable: Writable;
  onEvent(fn: (ev: BenchEvent & { pi?: string }) => void): () => void;   // every session's events + list/exchange/task/proc/writable changes
  start(): Promise<void>;                     // marks lost, ensures ≥1 session, reopens non-archived (unless readOnly)
  stop(): void;
  create(): Promise<SessionRow>;              // 409-ish Error when readOnly
  rpc(id: string, cmd: Record<string, unknown>): Promise<PiEvent>;   // refuses prompt when !writable, any when readOnly
  messages(id: string, after?: number, limit?: number): Promise<{ messages: unknown[]; total: number }>;
  archive(id: string): Promise<SessionRow>;
  restore(id: string): Promise<SessionRow>;
  remove(id: string, stop: boolean): Promise<void>;   // Error "in flight: …" when !stop and something runs
  busy(): boolean;                            // a turn in flight, a running/background task, or a live process
}
```

**How events fold, per session child:**

| pi event | effect |
|---|---|
| `response` to `get_state` / `new_session` / `switch_session` carrying `data.sessionFile` | `sessions.update(id, {file})` |
| first `prompt` on a session named `session N` | rename to the prompt's first 40 chars |
| `agent_start`, `prompt` | `lastActive = now` |
| `tool_execution_start` | `tasks.transition({id: toolCallId, session, tool, arg, state:"running", started})` |
| `tool_execution_end` | `background` + `n` when the result starts `Sent to the background as task #N`, else `done`/`failed` |
| `message_end` custom `background-task` | the `#N` task goes to `done`/`failed` |
| `extension_ui_request` `setWidget` `harness:procs` | `procs.snapshot(session, rows)` |
| `extension_ui_request` `setWidget` `harness:exchange` | `exchanges.record({...row, session})` or `transition` when only `{id,state}` |

Every harness file write goes through `writable.run`. After a fold, the Bench emits `{type:"sessions"}`, `{type:"exchange", row}`, `{type:"task", row}` or `{type:"procs", rows}` on the bus. RPC events go out as-is with `pi: id`, the same shape `live.onEvent` already takes.

- [ ] **Step 1: Write the failing test**

`harness/bench/test/bench.test.ts`:
```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";
import { FAKE } from "./rpc-child.test.ts";

const mk = (dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-core-")), readOnly = false) => new Bench({ dir, readOnly, model: "fake/m", bin: FAKE });
const settle = () => new Promise((r) => setTimeout(r, 150));

test("start on an empty folder opens one session and records its file", async () => {
  const b = mk();
  await b.start();
  await settle();
  const [s] = b.sessions.all();
  assert.equal(s.id, "s-1");
  assert.match(s.file ?? "", /\.jsonl$/);
  b.stop();
});

test("a prompt names the session and a widget exchange lands in both views", async () => {
  const b = mk();
  await b.start();
  const seen: string[] = [];
  b.onEvent((e) => seen.push(e.type));
  await b.rpc("s-1", { type: "prompt", message: "exchange" });
  await settle();
  assert.equal(b.sessions.get("s-1")!.name, "exchange");
  assert.deepEqual(b.exchanges.bySession("s-1").map((e) => e.id), ["e1"]);
  assert.deepEqual(b.exchanges.byWorkspace("api").map((e) => e.session), ["s-1"]);
  assert.ok(seen.includes("exchange") && seen.includes("agent_end"));
  b.stop();
});

test("a restart reopens sessions from their files and reads history", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-core-"));
  const a = mk(dir);
  await a.start();
  await settle();
  const file = a.sessions.get("s-1")!.file;
  a.stop();
  await settle();
  const b = mk(dir);
  await b.start();
  await settle();
  assert.equal(b.sessions.get("s-1")!.file, file);
  b.stop();
});

test("read-only serves history and refuses work", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-core-"));
  fs.mkdirSync(path.join(dir, "sessions"));
  fs.writeFileSync(path.join(dir, "sessions.json"), JSON.stringify([{ id: "s-1", name: "old", seq: 1, created: 1, lastActive: 1, archived: false, file: path.join(dir, "sessions", "x.jsonl") }]));
  fs.writeFileSync(path.join(dir, "sessions", "x.jsonl"), [
    { type: "session", version: 3, id: "x", timestamp: new Date().toISOString(), cwd: "/home/kl" },
    { type: "message", id: "m1", parentId: null, timestamp: new Date().toISOString(), message: { role: "user", content: "earlier", timestamp: 1 } },
  ].map((l) => JSON.stringify(l)).join("\n") + "\n");
  const b = mk(dir, true);
  await b.start();
  assert.equal((await b.messages("s-1")).total, 1);
  await assert.rejects(b.rpc("s-1", { type: "prompt", message: "x" }), /read-only/);
  await assert.rejects(b.create(), /read-only/);
  b.stop();
});

test("busy covers a turn in flight, a background task and a live process, and nothing else", async () => {
  const b = mk();
  await b.start();
  await settle();
  assert.equal(b.busy(), false, "an open session with nothing running is not busy");
  const turn = new Promise((r) => b.onEvent((e) => e.type === "agent_start" && r(b.busy())));
  await b.rpc("s-1", { type: "prompt", message: "hi" });
  assert.equal(await turn, true, "a turn is busy from agent_start");
  await settle();
  assert.equal(b.busy(), false, "and idle again after agent_end");
  b.tasks.transition({ id: "t1", session: "s-1", tool: "Bash", arg: "make", state: "background", started: 1 });
  assert.equal(b.busy(), true);
  b.tasks.transition({ id: "t1", state: "done", ended: 2 });
  b.procs.snapshot("s-1", [{ id: "p1", name: "vite", command: "npm run dev", pid: process.pid, started: 1 }]);
  assert.equal(b.busy(), true);
  b.procs.snapshot("s-1", []);
  assert.equal(b.busy(), false);
  b.stop();
});

test("delete refuses while something runs unless stop, then discards exchanges", async () => {
  const b = mk();
  await b.start();
  await b.rpc("s-1", { type: "prompt", message: "exchange" });
  await settle();
  b.tasks.transition({ id: "t1", session: "s-1", tool: "Bash", arg: "make", state: "background", started: 1 });
  await assert.rejects(b.remove("s-1", false), /in flight: Bash make/);
  await b.remove("s-1", true);
  assert.equal(b.sessions.get("s-1"), undefined);
  assert.equal(b.sessions.all().length, 1, "the last session is replaced, never zero");
  assert.deepEqual(b.exchanges.byWorkspace("api"), []);
  b.stop();
});

test("a prompt is refused while the folder is not writable", async () => {
  const b = mk();
  await b.start();
  assert.throws(() => b.writable.run(() => { throw new Error("EIO"); }));
  await assert.rejects(b.rpc("s-1", { type: "prompt", message: "x" }), /not writable: EIO/);
  b.stop();
});
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cd harness && node --test bench/test/bench.test.ts`
Expected: FAIL with `Cannot find module '.../bench/src/bench.ts'`.

- [ ] **Step 3: Implement**

`harness/bench/src/bench.ts`:
```ts
import fs from "node:fs";
import path from "node:path";
import { ExchangeLog, type Exchange } from "./exchanges.ts";
import { Writable } from "./guard.ts";
import { Procs, Tasks, type ProcRow } from "./ledger.ts";
import { page, transcript } from "./reader.ts";
import { RpcChild, type PiEvent } from "./rpc-child.ts";
import { SessionList, type SessionRow } from "./sessions.ts";

export type BenchEvent = { type: string; [k: string]: unknown };
export type BenchOpts = { dir: string; readOnly: boolean; model: string; bin?: string; extDir?: string };

const TOOL: Record<string, string> = { bash: "Bash", read: "Read", write: "Write", edit: "Edit", grep: "Grep", glob: "Glob", ls: "List" };
const argOf = (name: string, args: Record<string, unknown>) =>
  name === "bash" ? String(args.command ?? "") : String(args.path ?? args.file_path ?? args.pattern ?? JSON.stringify(args)).slice(0, 200);

/**
 * One person's bench in one team: the list, a pi per open session, and the
 * logs beside them. The only writer of the folder's harness files; pi writes
 * its own session JSONL. Every device is a view of this object.
 */
export class Bench {
  readonly sessions: SessionList;
  readonly exchanges: ExchangeLog;
  readonly tasks: Tasks;
  readonly procs: Procs;
  readonly writable: Writable;
  private opts: BenchOpts;
  private children = new Map<string, RpcChild>();
  private listeners = new Set<(ev: BenchEvent & { pi?: string }) => void>();
  /** Sessions between agent_start and agent_end: a turn nobody watches still holds the bench up. */
  private turning = new Set<string>();

  constructor(opts: BenchOpts) {
    this.opts = opts;
    fs.mkdirSync(path.join(opts.dir, "sessions"), { recursive: true });
    this.sessions = new SessionList(opts.dir);
    this.exchanges = new ExchangeLog(opts.dir);
    this.tasks = new Tasks(opts.dir);
    this.procs = new Procs(opts.dir);
    this.writable = new Writable(opts.dir, (ok, reason) => this.emit({ type: "writable", ok, reason }));
  }

  onEvent(fn: (ev: BenchEvent & { pi?: string }) => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }
  private emit(ev: BenchEvent & { pi?: string }) {
    for (const fn of this.listeners) fn(ev);
  }
  private write<T>(fn: () => T): T | undefined {
    try {
      return this.writable.run(fn);
    } catch {
      return undefined; // the guard has flipped and said so on the bus
    }
  }

  async start(): Promise<void> {
    if (!this.opts.readOnly) {
      // A new process holds none of the old one's children.
      this.write(() => this.tasks.markLost()).forEach?.((row) => this.emit({ type: "task", row }));
      const lost = this.write(() => this.procs.markLost());
      if (lost?.length) this.emit({ type: "procs", rows: this.procs.all() });
      if (!this.sessions.all().some((s) => !s.archived)) this.write(() => this.sessions.create(this.opts.model));
      for (const s of this.sessions.all().filter((x) => !x.archived)) this.open(s);
    }
  }

  stop(): void {
    for (const c of this.children.values()) c.stop();
    this.children.clear();
  }

  private open(s: SessionRow): RpcChild {
    let c = this.children.get(s.id);
    if (c?.running()) return c;
    const file = s.file && fs.existsSync(s.file) ? s.file : undefined;
    c = new RpcChild(s.id, { dir: path.join(this.opts.dir, "sessions"), file, model: s.model ?? this.opts.model, bin: this.opts.bin, extDir: this.opts.extDir }, (ev) => this.fold(s.id, ev));
    this.children.set(s.id, c);
    c.start();
    // The file name is pi's to choose; ask once so the list can reopen it.
    void c.send({ type: "get_state" }).catch(() => undefined);
    return c;
  }

  private fold(id: string, ev: PiEvent) {
    const now = Date.now();
    const data = ev.data as { sessionFile?: string } | undefined;
    if (ev.type === "response" && typeof data?.sessionFile === "string" && this.sessions.get(id)?.file !== data.sessionFile) {
      this.write(() => this.sessions.update(id, { file: data.sessionFile }));
      this.emit({ type: "sessions" });
    }
    if (ev.type === "agent_start") {
      this.turning.add(id);
      this.write(() => this.sessions.update(id, { lastActive: now }));
    }
    if (ev.type === "agent_end" || ev.type === "exit") this.turning.delete(id);
    if (ev.type === "tool_execution_start") {
      const name = ev.toolName as string;
      const row = this.write(() => this.tasks.transition({ id: ev.toolCallId as string, session: id, tool: TOOL[name] ?? name, arg: argOf(name, ev.args as Record<string, unknown>), state: "running", started: now }));
      if (row) this.emit({ type: "task", row });
    }
    if (ev.type === "tool_execution_end") {
      const out = ((ev.result as { content?: { text?: string }[] } | undefined)?.content ?? []).map((c) => c.text ?? "").join("");
      const bg = /^Sent to the background as task #(\d+)/.exec(out);
      const row = this.write(() => this.tasks.transition(bg ? { id: ev.toolCallId as string, n: Number(bg[1]), state: "background" } : { id: ev.toolCallId as string, state: ev.isError ? "failed" : "done", ended: now }));
      if (row) this.emit({ type: "task", row });
    }
    const m = ev.message as { role?: string; customType?: string; content?: string } | undefined;
    if (ev.type === "message_end" && m?.role === "custom" && m.customType === "background-task" && typeof m.content === "string") {
      const n = Number(/#(\d+)/.exec(m.content)?.[1]);
      const t = this.tasks.all().find((x) => x.session === id && x.n === n);
      const row = t && this.write(() => this.tasks.transition({ id: t.id, state: /exit [1-9]/.test(m.content!.split("\n")[0]) ? "failed" : "done", ended: now }));
      if (row) this.emit({ type: "task", row });
    }
    if (ev.type === "extension_ui_request" && ev.method === "setWidget") {
      const line = (ev.widgetLines as string[] | undefined)?.[0];
      try {
        if (ev.widgetKey === "harness:procs") {
          const rows = (line ? JSON.parse(line) : []) as Omit<ProcRow, "session">[];
          if (this.write(() => this.procs.snapshot(id, rows)) !== undefined || this.writable.ok()) this.emit({ type: "procs", rows: this.procs.all() });
        }
        if (ev.widgetKey === "harness:exchange" && line) {
          const x = JSON.parse(line) as Partial<Exchange> & { id: string };
          const known = this.exchanges.bySession(id).some((e) => e.id === x.id);
          if (known && x.state) this.write(() => this.exchanges.transition(x.id, x.state!));
          else if (x.workspace && x.dir) this.write(() => this.exchanges.record({ id: x.id, session: id, workspace: x.workspace!, dir: x.dir!, text: x.text ?? "", state: x.state ?? "sent", ref: x.ref }));
          this.emit({ type: "exchange", row: this.exchanges.bySession(id).find((e) => e.id === x.id) });
        }
      } catch {
        /* a widget line that is not ours */
      }
    }
    this.emit({ ...ev, pi: id });
  }

  /** What the idle clock (idle.ts) asks: is anything running that a client leaving must not stop? */
  busy(): boolean {
    return (
      this.turning.size > 0 ||
      this.tasks.all().some((t) => t.state === "running" || t.state === "background") ||
      this.procs.all().some((p) => p.ended === undefined)
    );
  }

  private refuse(write: boolean) {
    if (this.opts.readOnly) throw new Error("this bench is read-only: you are no longer in this team, so it reads history and runs nothing");
    if (write && !this.writable.ok()) throw new Error(`the bench folder is not writable: ${this.writable.reason()}; prompts are refused until it is`);
  }

  async create(): Promise<SessionRow> {
    this.refuse(true);
    const s = this.writable.run(() => this.sessions.create(this.opts.model));
    this.open(s);
    this.emit({ type: "sessions" });
    return s;
  }

  async rpc(id: string, cmd: Record<string, unknown>): Promise<PiEvent> {
    const s = this.sessions.get(id);
    if (!s) throw new Error(`no session ${id}`);
    this.refuse(cmd.type === "prompt" || cmd.type === "steer" || cmd.type === "follow_up");
    if (s.archived) throw new Error(`session ${id} is archived; restore it to send`);
    if (cmd.type === "prompt" && typeof cmd.message === "string" && s.name === `session ${s.seq}` && !cmd.message.startsWith("/") && cmd.message.trim()) {
      this.write(() => this.sessions.update(id, { name: cmd.message.trim().replace(/\s+/g, " ").slice(0, 40), lastActive: Date.now() }));
      this.emit({ type: "sessions" });
    }
    return this.open(s).send(cmd);
  }

  async messages(id: string, after?: number, limit?: number) {
    const s = this.sessions.get(id);
    if (!s) throw new Error(`no session ${id}`);
    const c = this.children.get(id);
    if (c?.running()) {
      const r = await c.send({ type: "get_messages" });
      return page(((r.data as { messages?: unknown[] } | undefined)?.messages ?? []), after, limit);
    }
    return page(s.file && fs.existsSync(s.file) ? transcript(s.file) : [], after, limit);
  }

  async archive(id: string): Promise<SessionRow> {
    this.refuse(true);
    if (this.sessions.all().filter((s) => !s.archived).length < 2) throw new Error("this is the only open session; start another before archiving it");
    this.children.get(id)?.stop();
    this.children.delete(id);
    const s = this.writable.run(() => this.sessions.update(id, { archived: true }));
    this.emit({ type: "sessions" });
    return s;
  }

  async restore(id: string): Promise<SessionRow> {
    this.refuse(true);
    const s = this.writable.run(() => this.sessions.update(id, { archived: false, lastActive: Date.now() }));
    this.open(s);
    this.emit({ type: "sessions" });
    return s;
  }

  private inFlight(id: string): string[] {
    return [
      ...this.tasks.all().filter((t) => t.session === id && (t.state === "running" || t.state === "background")).map((t) => `${t.tool} ${t.arg}`),
      ...this.procs.all().filter((p) => p.session === id && p.ended === undefined).map((p) => `process ${p.name}`),
    ];
  }

  async remove(id: string, stop: boolean): Promise<void> {
    this.refuse(true);
    const s = this.sessions.get(id);
    if (!s) throw new Error(`no session ${id}`);
    const items = this.inFlight(id);
    if (items.length && !stop) throw new Error(`in flight: ${items.join(", ")}`);
    const c = this.children.get(id);
    if (c?.running()) {
      // Commands and processes run in their own process groups: stop them
      // through pi before pi goes, or they outlive the session.
      for (const t of this.tasks.all().filter((t) => t.session === id && t.state === "background")) await c.send({ type: "prompt", message: `/cancel #${t.n}` }).catch(() => undefined);
      for (const p of this.procs.all().filter((p) => p.session === id && p.ended === undefined)) await c.send({ type: "prompt", message: `/proc-stop ${p.id}` }).catch(() => undefined);
      await c.send({ type: "abort" }).catch(() => undefined);
      c.stop();
    }
    this.children.delete(id);
    this.writable.run(() => {
      for (const t of this.tasks.all().filter((t) => t.session === id && (t.state === "running" || t.state === "background"))) this.tasks.transition({ id: t.id, state: "cancelled", ended: Date.now() });
      this.exchanges.discard(id);
      fs.rmSync(path.join(this.opts.dir, "btw", id), { recursive: true, force: true });
      if (s.file && fs.existsSync(s.file)) {
        const trash = path.join(this.opts.dir, "sessions", ".trash");
        fs.mkdirSync(trash, { recursive: true });
        fs.renameSync(s.file, path.join(trash, path.basename(s.file)));
      }
      this.sessions.remove(id);
      if (!this.sessions.all().some((x) => !x.archived)) this.open(this.sessions.create(this.opts.model));
    });
    this.emit({ type: "sessions" });
    this.emit({ type: "procs", rows: this.procs.all() });
  }
}
```

In `start()`, replace the `.forEach?.` line with a plain form so the lost-task path reads straight:
```ts
const lostTasks = this.write(() => this.tasks.markLost()) ?? [];
for (const row of lostTasks) this.emit({ type: "task", row });
```

- [ ] **Step 4: Run it and watch it pass**

Run: `cd harness && node --test bench/test/bench.test.ts`
Expected: `# pass 7`, `# fail 0`.

- [ ] **Step 5: Commit**

```bash
git add harness/bench/src/bench.ts harness/bench/test/bench.test.ts
git commit -m "Supervise bench sessions and fold their events into the folder"
```

---

### Task 10: `/btw` and import on the Bench

**Files:**
- Modify: `harness/bench/src/bench.ts`
- Create: `harness/bench/test/btw-import.test.ts`

**Interfaces:**
```ts
// on Bench
btw(session: string, question: string): Promise<{ id: string; question: string; entries: unknown[]; at: number }>;
listBtw(session: string): { id: string; question: string; entries: unknown[]; at: number }[];
import(items: { row: SessionRow; name: string; content?: string }[], loose: { name: string; content: string }[]): { added: string[]; files: number };
```

`btw` forks the session's file into a read-only child. The child is named `btw-N`, with N being the count of files under `btw/` plus 1. The fork takes the prompt and is resolved on `agent_end`. `btw` then reads `get_messages`, keeps only the entries after the fork point, writes `btw/{session}/{id}.json`, and stops the child. The child's events go on the bus with `pi: "btw-N"`, so a device watching streams the answer.

`import` works like this:

- Session files are written as `sessions/<name>`, but only when that name does not already exist.
- A row whose `file` is set is re-pointed at that path under the bench folder.
- Rows go through `sessions.merge`, so a re-run adds nothing and writes nothing.
- Loose files are written the same way and get no row.

- [ ] **Step 1: Write the failing test**

`harness/bench/test/btw-import.test.ts`:
```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";
import { FAKE } from "./rpc-child.test.ts";

const mk = () => new Bench({ dir: fs.mkdtempSync(path.join(os.tmpdir(), "bench-bi-")), readOnly: false, model: "fake/m", bin: FAKE });
const settle = () => new Promise((r) => setTimeout(r, 150));

test("btw answers from a fork, is kept under btw/, and its child is gone", async () => {
  const b = mk();
  await b.start();
  await settle();
  const a = await b.btw("s-1", "what is this");
  assert.equal(a.id, "btw-1");
  assert.deepEqual((a.entries as { role: string }[]).map((m) => m.role), ["user", "assistant"]);
  assert.deepEqual(b.listBtw("s-1").map((x) => x.question), ["what is this"]);
  b.stop();
});

test("import copies files, merges rows, and a re-run is a no-op", async () => {
  const b = mk();
  const row = { id: "bench", name: "fix login", seq: 1, created: 1, lastActive: 1, archived: false, file: "/Users/x/.pi/agent/sessions/--h--/a.jsonl" };
  const content = JSON.stringify({ type: "session", version: 3, id: "a", timestamp: new Date().toISOString(), cwd: "/Users/x" }) + "\n";
  const first = b.import([{ row, name: "a.jsonl", content }], [{ name: "loose.jsonl", content }]);
  assert.deepEqual(first, { added: ["bench"], files: 2 });
  assert.equal(b.sessions.get("bench")!.file, path.join((b as unknown as { opts: { dir: string } }).opts.dir, "sessions", "a.jsonl"));
  assert.deepEqual(b.import([{ row, name: "a.jsonl", content }], [{ name: "loose.jsonl", content }]), { added: [], files: 0 });
  await b.start();
  assert.ok(!b.sessions.all().some((s) => s.id === "s-1"), "an imported live session means no fresh one");
  b.stop();
});
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cd harness && node --test bench/test/btw-import.test.ts`
Expected: FAIL with `TypeError: b.btw is not a function`.

- [ ] **Step 3: Implement**

Add these methods to `Bench` in `harness/bench/src/bench.ts`:
```ts
  async btw(session: string, question: string) {
    this.refuse(true);
    const s = this.sessions.get(session);
    if (!s?.file || !fs.existsSync(s.file)) throw new Error("this session has no file yet; say something first");
    const dir = path.join(this.opts.dir, "btw", session);
    const id = `btw-${(fs.existsSync(dir) ? fs.readdirSync(dir).length : 0) + 1}`;
    const forkDir = path.join(this.opts.dir, "btw", ".forks");
    fs.mkdirSync(forkDir, { recursive: true });
    let done!: () => void;
    const ended = new Promise<void>((r) => (done = r));
    const child = new RpcChild(id, { dir: forkDir, fork: s.file, model: s.model ?? this.opts.model, bin: this.opts.bin }, (ev) => {
      this.emit({ ...ev, pi: id });
      if (ev.type === "agent_end" || ev.type === "exit") done();
    });
    child.start();
    try {
      const before = ((await child.send({ type: "get_messages" })).data as { messages?: unknown[] } | undefined)?.messages?.length ?? 0;
      await child.send({ type: "prompt", message: question });
      await ended;
      const all = ((await child.send({ type: "get_messages" })).data as { messages?: unknown[] } | undefined)?.messages ?? [];
      const answer = { id, question, entries: all.slice(before), at: Date.now() };
      this.writable.run(() => replaceJson(path.join(dir, `${id}.json`), answer));
      return answer;
    } finally {
      child.stop();
    }
  }

  listBtw(session: string) {
    const dir = path.join(this.opts.dir, "btw", session);
    if (!fs.existsSync(dir)) return [];
    return fs.readdirSync(dir).filter((f) => f.endsWith(".json")).map((f) => readJson(path.join(dir, f), null)).filter((x) => x !== null)
      .sort((a, b) => (a as { at: number }).at - (b as { at: number }).at) as { id: string; question: string; entries: unknown[]; at: number }[];
  }

  import(items: { row: SessionRow; name: string; content?: string }[], loose: { name: string; content: string }[]) {
    this.refuse(true);
    const dir = path.join(this.opts.dir, "sessions");
    let files = 0;
    const put = (name: string, content: string) => {
      const safe = path.basename(name);
      const to = path.join(dir, safe);
      if (!safe.endsWith(".jsonl") || fs.existsSync(to)) return to;
      fs.writeFileSync(to, content, { flag: "wx" });
      files++;
      return to;
    };
    return this.writable.run(() => {
      const rows = items.map(({ row, name, content }) => ({ ...row, archived: !!row.archived, file: content !== undefined ? put(name, content) : undefined }));
      for (const f of loose) put(f.name, f.content);
      const added = this.sessions.merge(rows);
      if (added.length) this.emit({ type: "sessions" });
      return { added, files };
    });
  }
```
Add `readJson, replaceJson` to the imports at the top: `import { readJson, replaceJson } from "./log.ts";`.

The fake pi ignores `--fork` and starts empty, so `before` is 0 and `entries` holds exactly the question and its answer. Real pi's fork carries the parent's messages, and slicing at `before` drops them.

- [ ] **Step 4: Run it and watch it pass**

Run: `cd harness && node --test bench/test/btw-import.test.ts`
Expected: `# pass 2`, `# fail 0`.

- [ ] **Step 5: Commit**

```bash
git add harness/bench/src/bench.ts harness/bench/test/btw-import.test.ts
git commit -m "Answer btw from a read-only fork and import laptop sessions idempotently"
```

---

### Task 11: The HTTP and WebSocket surface

**Files:**
- Create: `harness/bench/src/server.ts`, `harness/bench/test/server.test.ts`

**Interfaces:**
```ts
export function serve(bench: Bench, port: number, host?: string, idle?: Idle): Promise<{ port: number; close(): Promise<void> }>;
```

Every WebSocket, `/events` or a session's `/rpc`, calls `idle.opened()` on upgrade and `idle.closed()` on close; `serve` makes its own `Idle` over `bench.busy` when none is passed, so the tests need none.

| Route | Answer |
|---|---|
| `GET /healthz` | `{ok, readOnly, writable, reason?, clients, busy, idleSince}` (`idleSince` a ms timestamp or `null`) |
| `GET /sessions` | `SessionRow[]` |
| `POST /sessions` | `SessionRow` (201) |
| `DELETE /sessions/{id}` body `{stop?:boolean}` | 204; 409 `{error:"in flight: …", items}` |
| `POST /sessions/{id}/archive` · `/restore` | `SessionRow` |
| `GET /sessions/{id}/messages?after=&limit=` | `{messages, total}` |
| `POST /sessions/{id}/btw` `{question}` | answer; `GET` lists |
| `GET /exchanges?session=|workspace=&after=` | `Exchange[]`; neither param → 400 |
| `GET /workspaces/{ws}/messages?after=` | `byWorkspace` for now; Task 20 serves the workspace's own thread here |
| `GET /tasks` · `GET /procs` | rows |
| `POST /import` `{items, loose}` | `{added, files}` |
| `WS /sessions/{id}/rpc` | client frames `{id, ...cmd}`; the response comes back to that client only with the client's `id`; every event of the session goes to every client of it |
| `WS /events` | every Bench event, one JSON frame each |

An error from `Bench` answers 409 when it says `read-only`, `not writable`, `in flight` or `only open session`, 404 on `no session`, and 400 otherwise. Every body is `{error}`.

- [ ] **Step 1: Write the failing test**

`harness/bench/test/server.test.ts`:
```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import WebSocket from "ws";
import { Bench } from "../src/bench.ts";
import { serve } from "../src/server.ts";
import { FAKE } from "./rpc-child.test.ts";

async function up() {
  const bench = new Bench({ dir: fs.mkdtempSync(path.join(os.tmpdir(), "bench-srv-")), readOnly: false, model: "fake/m", bin: FAKE });
  await bench.start();
  const srv = await serve(bench, 0);
  const base = `http://127.0.0.1:${srv.port}`;
  return { bench, srv, base, ws: (p: string) => new WebSocket(`ws://127.0.0.1:${srv.port}${p}`), down: async () => (bench.stop(), await srv.close()) };
}
const opened = (w: WebSocket) => new Promise((r) => w.once("open", r));
const frames = (w: WebSocket, out: Record<string, unknown>[]) => w.on("message", (d) => out.push(JSON.parse(d.toString())));
const settle = () => new Promise((r) => setTimeout(r, 200));

test("two clients on one session see the same events in the same order; responses go to their sender", async () => {
  const t = await up();
  const a = t.ws("/sessions/s-1/rpc"), b = t.ws("/sessions/s-1/rpc");
  await Promise.all([opened(a), opened(b)]);
  const fa: Record<string, unknown>[] = [], fb: Record<string, unknown>[] = [];
  frames(a, fa);
  frames(b, fb);
  a.send(JSON.stringify({ id: "1", type: "prompt", message: "hi" }));
  b.send(JSON.stringify({ id: "1", type: "get_state" }));
  await settle();
  const events = (f: Record<string, unknown>[]) => f.filter((x) => x.type !== "response").map((x) => x.type);
  assert.deepEqual(events(fa), events(fb));
  assert.deepEqual(events(fa), ["agent_start", "message_update", "agent_end"]);
  assert.deepEqual(fa.filter((x) => x.type === "response").map((x) => x.command), ["prompt"]);
  assert.deepEqual(fb.filter((x) => x.type === "response").map((x) => x.command), ["get_state"]);
  assert.ok(fa.every((x) => x.type !== "response" || x.id === "1"));
  a.close(); b.close();
  await t.down();
});

test("REST: list, create, messages, archive, exchanges by both views, delete", async () => {
  const t = await up();
  const quiet = await (await fetch(t.base + "/healthz")).json();
  assert.equal(quiet.clients, 0);
  assert.equal(typeof quiet.idleSince, "number", "no client and nothing running is idle");
  const ev = t.ws("/events");
  await opened(ev);
  const held = await (await fetch(t.base + "/healthz")).json();
  assert.equal(held.clients, 1);
  assert.equal(held.idleSince, null, "a connected client holds the bench up");
  const seen: Record<string, unknown>[] = [];
  frames(ev, seen);
  const j = async (method: string, p: string, body?: unknown) => {
    const r = await fetch(t.base + p, { method, headers: body ? { "content-type": "application/json" } : {}, body: body ? JSON.stringify(body) : undefined });
    return { status: r.status, body: r.status === 204 ? null : await r.json() };
  };
  assert.equal((await j("POST", "/sessions")).status, 201);
  assert.deepEqual((await j("GET", "/sessions")).body.map((s: { id: string }) => s.id), ["s-1", "s-2"]);
  await t.bench.rpc("s-1", { type: "prompt", message: "exchange" });
  await settle();
  assert.equal((await j("GET", "/sessions/s-1/messages?after=1&limit=5")).body.total, 2);
  assert.equal((await j("GET", "/exchanges?session=s-1")).body[0].workspace, "api");
  assert.equal((await j("GET", "/exchanges?workspace=api")).body[0].session, "s-1");
  assert.equal((await j("GET", "/exchanges")).status, 400);
  assert.equal((await j("POST", "/sessions/s-2/archive")).body.archived, true);
  assert.equal((await j("POST", "/sessions/s-1/archive")).status, 409);
  assert.equal((await j("DELETE", "/sessions/nope", { stop: true })).status, 404);
  assert.equal((await j("DELETE", "/sessions/s-2", { stop: true })).status, 204);
  assert.ok(seen.some((e) => e.type === "sessions") && seen.some((e) => e.type === "exchange"));
  ev.close();
  await t.down();
});
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cd harness && node --test bench/test/server.test.ts`
Expected: FAIL with `Cannot find module '.../bench/src/server.ts'`.

- [ ] **Step 3: Implement**

`harness/bench/src/server.ts`:
```ts
import http from "node:http";
import { WebSocketServer, type WebSocket } from "ws";
import type { Bench } from "./bench.ts";
import { Idle } from "./idle.ts";

/**
 * harness-bench's surface. Where it listens is main's choice: the pod IP
 * behind the platform's gateway-only NetworkPolicy, or loopback on a laptop.
 * Each session's RPC is pi's own JSONL framing, one
 * message per frame; ids are the client's and are rewritten only for the trip
 * through pi, so two devices can both send id "1".
 */
const status = (e: Error) => (/no session/.test(e.message) ? 404 : /read-only|not writable|in flight|only open session|archived/.test(e.message) ? 409 : 400);

async function body(req: http.IncomingMessage): Promise<Record<string, unknown>> {
  let s = "";
  for await (const c of req) s += c;
  return s ? (JSON.parse(s) as Record<string, unknown>) : {};
}

export function serve(bench: Bench, port: number, host = "127.0.0.1", idle = new Idle(() => bench.busy())): Promise<{ port: number; close(): Promise<void> }> {
  const send = (res: http.ServerResponse, code: number, v?: unknown) => {
    res.writeHead(code, v === undefined ? {} : { "content-type": "application/json" });
    res.end(v === undefined ? undefined : JSON.stringify(v));
  };
  const server = http.createServer(async (req, res) => {
    const u = new URL(req.url ?? "/", "http://bench");
    const p = u.pathname.split("/").filter(Boolean).map(decodeURIComponent);
    const n = (k: string) => (u.searchParams.has(k) ? Number(u.searchParams.get(k)) : undefined);
    try {
      const m = req.method ?? "GET";
      if (m === "GET" && u.pathname === "/healthz") return send(res, 200, { ok: true, readOnly: (bench as unknown as { opts: { readOnly: boolean } }).opts.readOnly, writable: bench.writable.ok(), reason: bench.writable.reason(), ...idle.state() });
      if (p[0] === "sessions") {
        if (p.length === 1 && m === "GET") return send(res, 200, bench.sessions.all());
        if (p.length === 1 && m === "POST") return send(res, 201, await bench.create());
        if (p.length === 2 && m === "DELETE") {
          const b = await body(req);
          try {
            await bench.remove(p[1], b.stop === true);
            return send(res, 204);
          } catch (e) {
            const msg = (e as Error).message;
            if (msg.startsWith("in flight: ")) return send(res, 409, { error: msg, items: msg.slice(11).split(", ") });
            throw e;
          }
        }
        if (p.length === 3 && m === "POST" && p[2] === "archive") return send(res, 200, await bench.archive(p[1]));
        if (p.length === 3 && m === "POST" && p[2] === "restore") return send(res, 200, await bench.restore(p[1]));
        if (p.length === 3 && m === "GET" && p[2] === "messages") return send(res, 200, await bench.messages(p[1], n("after"), n("limit")));
        if (p.length === 3 && p[2] === "btw" && m === "POST") return send(res, 200, await bench.btw(p[1], String((await body(req)).question ?? "")));
        if (p.length === 3 && p[2] === "btw" && m === "GET") return send(res, 200, bench.listBtw(p[1]));
      }
      if (m === "GET" && u.pathname === "/exchanges") {
        const s = u.searchParams.get("session"), w = u.searchParams.get("workspace");
        if (!s === !w) return send(res, 400, { error: "exactly one of session= or workspace=" });
        return send(res, 200, s ? bench.exchanges.bySession(s, n("after")) : bench.exchanges.byWorkspace(w!, n("after")));
      }
      if (m === "GET" && p[0] === "workspaces" && p.length === 3 && p[2] === "messages") return send(res, 200, bench.exchanges.byWorkspace(p[1], n("after")));
      if (m === "GET" && u.pathname === "/tasks") return send(res, 200, bench.tasks.all());
      if (m === "GET" && u.pathname === "/procs") return send(res, 200, bench.procs.all());
      if (m === "POST" && u.pathname === "/import") {
        const b = await body(req);
        return send(res, 200, bench.import((b.items ?? []) as never, (b.loose ?? []) as never));
      }
      send(res, 404, { error: `no route ${m} ${u.pathname}` });
    } catch (e) {
      send(res, status(e as Error), { error: (e as Error).message });
    }
  });

  const wss = new WebSocketServer({ noServer: true });
  const events = new Set<WebSocket>();
  const rpcClients = new Map<string, Set<WebSocket>>();
  const pending = new Map<WebSocket, Map<string, string>>(); // pi-bound id → client id
  let seq = 0;
  bench.onEvent((ev) => {
    const frame = JSON.stringify(ev);
    for (const w of events) w.send(frame);
    if (!ev.pi || ev.type === "response") return;
    for (const w of rpcClients.get(ev.pi) ?? []) w.send(frame);
  });

  server.on("upgrade", (req, socket, head) => {
    const p = new URL(req.url ?? "/", "http://bench").pathname.split("/").filter(Boolean);
    const rpc = p.length === 3 && p[0] === "sessions" && p[2] === "rpc" ? p[1] : undefined;
    if (!rpc && !(p.length === 1 && p[0] === "events")) return void socket.destroy();
    wss.handleUpgrade(req, socket, head, (w) => {
      // A connected device holds the bench up whichever socket it holds.
      idle.opened();
      w.on("close", () => idle.closed());
      if (!rpc) {
        events.add(w);
        w.on("close", () => events.delete(w));
        return;
      }
      if (!rpcClients.has(rpc)) rpcClients.set(rpc, new Set());
      rpcClients.get(rpc)!.add(w);
      w.on("close", () => rpcClients.get(rpc)?.delete(w));
      w.on("message", async (d) => {
        let cmd: Record<string, unknown>;
        try {
          cmd = JSON.parse(d.toString()) as Record<string, unknown>;
        } catch {
          return void w.send(JSON.stringify({ type: "response", success: false, error: "not JSON" }));
        }
        const clientId = cmd.id;
        const { id: _drop, ...rest } = cmd;
        try {
          const r = await bench.rpc(rpc, rest);
          w.send(JSON.stringify({ ...r, id: clientId }));
        } catch (e) {
          w.send(JSON.stringify({ type: "response", id: clientId, command: cmd.type, success: false, error: (e as Error).message }));
        }
      });
    });
  });

  return new Promise((resolve) => {
    server.listen(port, host, () => {
      const addr = server.address() as { port: number };
      resolve({
        port: addr.port,
        close: () => new Promise<void>((r) => {
          for (const w of wss.clients) w.terminate();
          server.close(() => r());
        }),
      });
    });
  });
}
```

Delete the unused `pending` and `seq` declarations. `RpcChild` already mints its own ids and the server maps back through the awaited promise, so neither is needed.

- [ ] **Step 4: Run it and watch it pass**

Run: `cd harness && node --test bench/test/server.test.ts`
Expected: `# pass 2`, `# fail 0`.

- [ ] **Step 5: Commit**

```bash
git add harness/bench/src/server.ts harness/bench/test/server.test.ts
git commit -m "Serve bench sessions, exchanges and RPC fan-out"
```

---

### Task 12: The `harness-bench` entry point

**Files:**
- Create: `harness/bench/src/idle.ts`, `harness/bench/test/idle.test.ts`, `harness/bench/src/main.ts`, `harness/bench/test/main.test.ts`

**Interfaces:**
```
node bench/src/main.ts --dir /bench [--host 0.0.0.0] [--port 7789] [--read-only] [--wait] [--idle-secs N] [--model deepseek/deepseek-v4-flash]
node bench/src/main.ts --ping [--port 7789]
stdout: one line `harness-bench listening on <host>:<port> (running|read-only) dir=<dir>`
exit 75 when the lock is held, the holder written to $TERMINATION_LOG (default /dev/termination-log) when that file can be written
exit 0 with "idle" written to $TERMINATION_LOG once no client has been connected and nothing has run for N seconds
--wait: wait for the lock instead, logging "waiting on <holder>"
--idle-secs: default $KL_BENCH_IDLE_SECS, else 0; 0 never exits idle
--ping: exit 0 when GET http://127.0.0.1:<port>/healthz answers ok, else 1
```

Start order is lock, then `Bench.start()`, then truncating `.health`, then `serve` with one `Idle`, then the writable probe beat every 10 s and the idle beat every 5 s. SIGTERM, and an idle exit, stop the children, close the server, release the lock and exit 0. `--read-only` takes no lock: it writes nothing, and a reader beside a writer is fine. A read-only bench idles out like any other.

These defaults are the platform's (decisions 11 and 12). The pod runs `harness-bench` with no flags but `KL_BENCH_IDLE_SECS` in its env; its readiness probe is `harness-bench --ping`; the agent reads exit 75 as `FolderLocked` and a `Succeeded` pod as asleep.

The idle clock is its own small file, tested before the entry point that uses it.

`harness/bench/test/idle.test.ts`:
```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import { Idle } from "../src/idle.ts";

test("idle starts at zero clients and no work, and any client or work clears it", () => {
  let t = 1000;
  let busy = false;
  const i = new Idle(() => busy, () => t);
  assert.equal(i.state().idleSince, 1000, "a fresh bench with nobody on it is idle from the start");
  t = 2000;
  i.opened();
  assert.equal(i.state().idleSince, null);
  t = 3000;
  i.closed();
  assert.equal(i.state().idleSince, 3000, "idle from the moment the last client left");
  t = 3500;
  busy = true;
  assert.equal(i.idleFor(), 0, "a turn with nobody watching holds it up");
  t = 4000;
  busy = false;
  assert.equal(i.state().idleSince, 4000, "the clock restarts when the work ends, it does not resume");
  t = 9000;
  assert.equal(i.idleFor(), 5000);
  i.closed();
  assert.equal(i.state().clients, 0, "a stray close never goes negative");
});
```

`harness/bench/src/idle.ts`:
```ts
/**
 * When a bench may sleep: nobody connected and nothing running. The platform
 * scales an idle bench to zero (the pod exits 0 and is not replaced until a
 * client connects), so a tool that runs with the laptop shut must hold it up,
 * which is why busy() is asked rather than only counting sockets.
 */
export class Idle {
  private clients = 0;
  private since: number | undefined;
  constructor(private busy: () => boolean, private now: () => number = Date.now) {
    this.since = this.now();
  }
  opened(): void {
    this.clients++;
    this.since = undefined;
  }
  closed(): void {
    this.clients = Math.max(0, this.clients - 1);
    this.tick();
  }
  private tick(): number | undefined {
    if (this.clients > 0 || this.busy()) this.since = undefined;
    else this.since ??= this.now();
    return this.since;
  }
  idleFor(): number {
    const s = this.tick();
    return s === undefined ? 0 : this.now() - s;
  }
  state(): { clients: number; busy: boolean; idleSince: number | null } {
    return { clients: this.clients, busy: this.busy(), idleSince: this.tick() ?? null };
  }
}
```

- [ ] **Step 1: Write the failing test**

`harness/bench/test/main.test.ts`:
```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { FAKE } from "./rpc-child.test.ts";

const MAIN = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "src", "main.ts");
const run = (args: string[], env: Record<string, string> = {}) => {
  const c = spawn(process.execPath, [MAIN, ...args], { env: { ...process.env, HARNESS_PI_BIN: FAKE, ...env } });
  let out = "";
  c.stdout.on("data", (d) => (out += d));
  c.stderr.on("data", (d) => (out += d));
  return { c, out: () => out, line: (re: RegExp) => new Promise<string>((r) => { const t = setInterval(() => { if (re.test(out)) (clearInterval(t), r(out)); }, 20); }) };
};
const exited = (c: ReturnType<typeof spawn>) => new Promise<number | null>((r) => c.on("exit", r));

test("a second writer exits 75 naming the holder; a reader beside it is served and answers --ping", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-main-"));
  const term = path.join(dir, "termination-log");
  fs.writeFileSync(term, "");
  const a = run(["--dir", dir, "--port", "0"], { NODE_NAME: "node-a" });
  await a.line(/listening on 0\.0\.0\.0:\d+ \(running\)/);

  const b = run(["--dir", dir, "--port", "0"], { TERMINATION_LOG: term });
  assert.equal(await exited(b.c), 75);
  assert.match(b.out(), /locked by node-a pid \d+/);
  assert.match(fs.readFileSync(term, "utf8"), /^node-a pid \d+$/);

  const r = run(["--dir", dir, "--port", "0", "--read-only"]);
  const port = Number(/0\.0\.0\.0:(\d+) \(read-only\)/.exec(await r.line(/\(read-only\)/))![1]);
  const rows = await (await fetch(`http://127.0.0.1:${port}/sessions`)).json();
  assert.equal(rows[0].id, "s-1");
  assert.equal(await exited(run(["--ping", "--port", String(port)]).c), 0);
  r.c.kill("SIGTERM");
  await exited(r.c);
  assert.equal(await exited(run(["--ping", "--port", String(port)]).c), 1, "nothing listening is not ready");

  a.c.kill("SIGTERM");
  assert.equal(await exited(a.c), 0);
});

test("with nobody connected and nothing running it exits 0 naming idle, and releases the lock", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-main-"));
  const term = path.join(dir, "termination-log");
  fs.writeFileSync(term, "");
  const a = run(["--dir", dir, "--port", "0"], { KL_BENCH_IDLE_SECS: "1", TERMINATION_LOG: term });
  const started = Date.now();
  assert.equal(await exited(a.c), 0);
  assert.ok(Date.now() - started < 15_000, "within the idle period plus two beats");
  assert.equal(fs.readFileSync(term, "utf8"), "idle");
  const b = run(["--dir", dir, "--port", "0"]);
  await b.line(/\(running\)/);
  b.c.kill("SIGTERM");
  assert.equal(await exited(b.c), 0, "the next start takes the lock at once");
});
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cd harness && node --test bench/test/idle.test.ts bench/test/main.test.ts`
Expected: FAIL with `Cannot find module '.../bench/src/idle.ts'`, and the main test times out because `main.ts` does not exist. `node:test` reports `Cannot find module` on the child's stderr.

- [ ] **Step 3: Implement**

`harness/bench/src/main.ts`:
```ts
import fs from "node:fs";
import path from "node:path";
import { parseArgs } from "node:util";
import { Bench } from "./bench.ts";
import { Idle } from "./idle.ts";
import { FolderLocked, takeLock, type Lock } from "./lock.ts";
import { serve } from "./server.ts";

const { values: a } = parseArgs({
  options: {
    dir: { type: "string", default: "/bench" },
    // The pod IP: the gateway dials it, and the platform's NetworkPolicy admits only the gateway.
    host: { type: "string", default: "0.0.0.0" },
    port: { type: "string", default: "7789" },
    model: { type: "string", default: process.env.HARNESS_PI_MODEL ?? "deepseek/deepseek-v4-flash" },
    "read-only": { type: "boolean", default: false },
    wait: { type: "boolean", default: false },
    ping: { type: "boolean", default: false },
    // The platform's benchIdleSecs, stamped into the pod; 0 (a laptop) never sleeps.
    "idle-secs": { type: "string", default: process.env.KL_BENCH_IDLE_SECS ?? "0" },
  },
});

if (a.ping) {
  // The pod's readiness probe: the same /healthz a client reads, over loopback.
  const ok = await fetch(`http://127.0.0.1:${a.port}/healthz`, { signal: AbortSignal.timeout(3000) }).then((r) => r.ok, () => false);
  process.exit(ok ? 0 : 1);
}

const dir = path.resolve(a.dir!);
const readOnly = a["read-only"]!;

let lock: Lock | undefined;
if (!readOnly) {
  try {
    lock = await takeLock(dir, { wait: a.wait, onWaiting: (h) => console.error(`harness-bench: waiting on ${h} for ${dir}/.lock`) });
  } catch (e) {
    console.error(`harness-bench: ${(e as Error).message}`);
    if (!(e instanceof FolderLocked)) process.exit(1);
    // Kubernetes keeps this file as the container's last message; the agent shows it as FolderLocked.
    try {
      fs.writeFileSync(process.env.TERMINATION_LOG ?? "/dev/termination-log", e.holder);
    } catch {
      /* not in a pod */
    }
    process.exit(75);
  }
}

const bench = new Bench({ dir, readOnly, model: a.model! });
await bench.start();
if (!readOnly) fs.writeFileSync(path.join(dir, ".health"), "");
const idle = new Idle(() => bench.busy());
const srv = await serve(bench, Number(a.port), a.host, idle);
console.log(`harness-bench listening on ${a.host}:${srv.port} (${readOnly ? "read-only" : "running"}) dir=${dir}`);
const beat = readOnly ? undefined : setInterval(() => bench.writable.probe(), 10_000);

let leaving = false;
const shutdown = async (why?: string) => {
  if (leaving) return;
  leaving = true;
  clearInterval(beat);
  clearInterval(sleep);
  if (why) {
    // The agent reads a Succeeded pod as asleep; the message says why for kubectl describe.
    try {
      fs.writeFileSync(process.env.TERMINATION_LOG ?? "/dev/termination-log", why);
    } catch {
      /* not in a pod */
    }
    console.error(`harness-bench: ${why}: no client and nothing running for ${a["idle-secs"]} s`);
  }
  bench.stop();
  await srv.close();
  lock?.release();
  process.exit(0);
};
const idleMs = Number(a["idle-secs"]) * 1000;
const sleep = setInterval(() => {
  if (idleMs > 0 && idle.idleFor() >= idleMs) void shutdown("idle");
}, 5_000);

process.on("SIGTERM", () => void shutdown());
```

- [ ] **Step 4: Run it and watch it pass**

Run: `cd harness && node --test bench/test/idle.test.ts bench/test/main.test.ts`
Expected: `# pass 3`, `# fail 0`.

- [ ] **Step 5: Run the whole suite**

Run: `cd harness && npm run bench:test`
Expected: `# fail 0`, with every file from Tasks 1–12 passing (`# pass 32`).

- [ ] **Step 6: Commit**

```bash
git add harness/bench/src/idle.ts harness/bench/test/idle.test.ts harness/bench/src/main.ts harness/bench/test/main.test.ts
git commit -m "Start harness-bench with the folder lock, a readiness ping, a read-only mode and an idle exit"
```

---

### Task 13: Extensions publish processes with pids and exchanges

**Files:**
- Modify: `harness/pi/process.ts` (the `snapshot` const), `harness/pi/kloudlite.ts` (the `reg` helper)

**Interfaces (widget payloads, one JSON string in `widgetLines[0]`):**
```ts
// harness:procs
{ id, name, command, pid?: number, started, ended?, code?, tail }[]
// harness:exchange
{ id: string, workspace: string, dir: "out" | "in", text: string, state: "sent" | "done" | "failed", ref?: string }  // first publish
{ id: string, state: "done" | "failed" }                                                                          // transition
```

There is no unit test runner for pi extensions. Verification runs the real extension through `harness-bench` with a real pi, in Step 3.

- [ ] **Step 1: Add `pid` to the process snapshot**

In `harness/pi/process.ts`, replace:
```ts
  const snapshot = () =>
    [...procs.values()].map((p) => ({ id: p.id, name: p.name, command: p.command, started: p.started, ended: p.ended, code: p.code, tail: tail(p, 40) }));
```
with:
```ts
  // The pid lets harness-bench tell, after its own restart, a process that
  // still runs from one that went with the old pod.
  const snapshot = () =>
    [...procs.values()].map((p) => ({ id: p.id, name: p.name, command: p.command, pid: p.proc.pid, started: p.started, ended: p.ended, code: p.code, tail: tail(p, 40) }));
```

- [ ] **Step 2: Publish exchanges around workspace calls**

In `harness/pi/kloudlite.ts`, replace the `reg` helper's `execute`:
```ts
      async execute(_id, a) {
        return run(a as Record<string, any>);
      },
```
with:
```ts
      async execute(toolCallId, a, _signal, _update, ctx) {
        const args = a as Record<string, any>;
        // A call that hands work to a workspace or environment is an exchange:
        // harness-bench records it in the bench's one log, where the session's
        // queue and the workspace's queue both read it.
        const target = /^kl_(workspace|environment)_/.test(name) ? String(args.id ?? args.name ?? "") : "";
        const publish = (v: unknown) => target && ctx?.ui?.setWidget("harness:exchange", [JSON.stringify(v)]);
        const id = `x-${toolCallId}`;
        publish({ id, workspace: target, dir: "out", text: `${name} ${JSON.stringify(args)}`, state: "sent" });
        const r = await run(args);
        publish({ id: `${id}-in`, workspace: target, dir: "in", text: r.content.map((c) => c.text).join("").slice(0, 2000), state: r.isError ? "failed" : "done", ref: id });
        publish({ id, state: r.isError ? "failed" : "done" });
        return r;
      },
```

- [ ] **Step 3: Exercise with a real pi**

This needs a provider key in the environment (`DEEPSEEK_API_KEY`) and a `/kl-login` token in `~/.config`.

Run:
```bash
cd harness && npm run typecheck
mkdir -p /tmp/bench-x && node bench/src/main.ts --dir /tmp/bench-x --host 127.0.0.1 --port 7790 &
sleep 3
node -e '
const W=require("ws");const w=new W("ws://127.0.0.1:7790/sessions/s-1/rpc");
w.on("open",()=>w.send(JSON.stringify({id:"1",type:"prompt",message:"Call kl_workspace_start with id nope-123 and tell me the answer."})));
w.on("message",d=>{const e=JSON.parse(d);if(e.type==="agent_end"){process.exit(0)}});'
curl -s 'http://127.0.0.1:7790/exchanges?workspace=nope-123'
kill %1
```
Expected:
- `typecheck` exits 0.
- `curl` prints a JSON array holding two rows for `nope-123`. One has `"dir":"out"` and `"state":"failed"`. The other has `"dir":"in"` and a `404` error text.

- [ ] **Step 4: Commit**

```bash
git add harness/pi/process.ts harness/pi/kloudlite.ts
git commit -m "Publish process pids and workspace exchanges from the bench extensions"
```

---

### Task 14: The harness main process talks to the bench

**Files:**
- Create: `harness/src/bench-client.ts`, `harness/bench/test/bench-client.test.ts`
- Modify: `harness/src/main.ts` (replace `Pi`, `pis`, `pi:spawn` and `pi:stop`), `harness/src/preload.ts`, `harness/tsconfig.main.json` (`include`)
- Delete: `harness/src/pi.ts`

**Interfaces:**
```ts
// harness/src/bench-client.ts (CommonJS, compiled by tsc like main.ts; no Electron import)
export type Emit = (ev: Record<string, unknown> & { type: string; pi?: string }) => void;
export class BenchClient {
  constructor(base: string, emit: Emit, cacheFile: string);
  connected(): boolean;
  start(): void;                                             // /events with reconnect (1 s → 15 s backoff)
  close(): void;
  rest<T = unknown>(method: string, path: string, body?: unknown): Promise<T>;   // throws Error(body.error) on ≥400
  rpc(session: string, cmd: Record<string, unknown>): Promise<Record<string, unknown>>;  // rejects "not connected" when offline
  messages(session: string): Promise<unknown[]>;             // cache + GET ?after=<cached length>; offline → cache
  cached(): { sessions: unknown[]; exchanges: unknown[] };
}
```

**Behaviour:**

- **Session sockets.** One WebSocket per session opens lazily on the first `rpc`. Events arriving on it are emitted as `pi:event` with `pi: session`, the shape `live.onEvent` already folds.
- **`/events`.** Only non-RPC events are emitted from `/events` (`sessions`, `exchange`, `task`, `procs`, `writable`). Session RPC events come from the session sockets, so each event reaches the renderer once.
- **Connection state.** A connection change is emitted as `{type:"bench", connected}`.
- **Reconnect.** On every reconnect, `sessions` is refreshed into the cache and `{type:"bench:resync"}` is emitted. The renderer then re-pages open sessions with `messages()`.
- **Prompts are never queued offline.** `rpc` while offline rejects at once with `not connected to the bench; nothing was sent`.

- [ ] **Step 1: Write the failing test**

It lives with the bench tests because it needs `harness-bench` and runs under `node --test`:

`harness/bench/test/bench-client.test.ts`:
```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";
import { serve } from "../src/server.ts";
import { FAKE } from "./rpc-child.test.ts";
import { BenchClient } from "../../src/bench-client.ts";

const settle = (ms = 250) => new Promise((r) => setTimeout(r, ms));

test("rpc streams as pi:event, offline refuses, reconnect resyncs and pages from the cache", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "bench-cl-"));
  const bench = new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE });
  await bench.start();
  let srv = await serve(bench, 0);
  const port = srv.port;
  const seen: Record<string, unknown>[] = [];
  const c = new BenchClient(`http://127.0.0.1:${port}`, (e) => seen.push(e), path.join(dir, "cache.json"));
  c.start();
  await settle();
  assert.equal(c.connected(), true);
  await c.rpc("s-1", { type: "prompt", message: "hi" });
  await settle();
  assert.deepEqual(seen.filter((e) => e.pi === "s-1" && e.type !== "response").map((e) => e.type), ["agent_start", "message_update", "agent_end"]);
  assert.equal((await c.messages("s-1")).length, 2);

  await srv.close();
  await settle();
  assert.equal(c.connected(), false);
  await assert.rejects(c.rpc("s-1", { type: "prompt", message: "lost?" }), /not connected/);
  assert.equal((await c.messages("s-1")).length, 2, "offline reads come from the cache");

  srv = await serve(bench, port);
  await settle(1500);
  assert.equal(c.connected(), true);
  assert.ok(seen.some((e) => e.type === "bench:resync"));
  c.close();
  bench.stop();
  await srv.close();
});
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cd harness && node --test bench/test/bench-client.test.ts`
Expected: FAIL with `Cannot find module '.../src/bench-client.ts'`.

- [ ] **Step 3: Implement the client**

`harness/src/bench-client.ts`:
```ts
import fs from "node:fs";
import WebSocket from "ws";

/**
 * The laptop side of a remote bench: every device is a view of one
 * harness-bench, reached at HARNESS_BENCH (the tunnel's local end). Sessions
 * stream over one WebSocket each; list, exchange, task and process changes
 * over /events. What was last seen is cached so a disconnected harness still
 * reads, and nothing typed while disconnected is ever queued here.
 */
export type Emit = (ev: Record<string, unknown> & { type: string; pi?: string }) => void;
type Cache = { sessions: unknown[]; exchanges: unknown[]; messages: Record<string, unknown[]> };
const KEEP_MESSAGES = 200;
const KEEP_EXCHANGES = 500;

export class BenchClient {
  private base: string;
  private emit: Emit;
  private cacheFile: string;
  private cache: Cache;
  private events?: WebSocket;
  private up = false;
  private closed = false;
  private backoff = 1000;
  private sockets = new Map<string, WebSocket>();
  private waiting = new Map<string, (r: Record<string, unknown>) => void>();
  private seq = 0;

  constructor(base: string, emit: Emit, cacheFile: string) {
    this.base = base.replace(/\/$/, "");
    this.emit = emit;
    this.cacheFile = cacheFile;
    try {
      this.cache = JSON.parse(fs.readFileSync(cacheFile, "utf8")) as Cache;
    } catch {
      this.cache = { sessions: [], exchanges: [], messages: {} };
    }
  }

  connected(): boolean {
    return this.up;
  }
  cached() {
    return { sessions: this.cache.sessions, exchanges: this.cache.exchanges };
  }
  private save() {
    try {
      fs.writeFileSync(`${this.cacheFile}.tmp`, JSON.stringify(this.cache));
      fs.renameSync(`${this.cacheFile}.tmp`, this.cacheFile);
    } catch {
      /* a cache that cannot be written is only a slower start */
    }
  }
  private setUp(v: boolean) {
    if (this.up === v) return;
    this.up = v;
    this.emit({ type: "bench", connected: v });
  }

  start(): void {
    if (this.closed) return;
    const w = new WebSocket(`${this.base.replace(/^http/, "ws")}/events`);
    this.events = w;
    w.on("open", async () => {
      this.backoff = 1000;
      this.setUp(true);
      try {
        this.cache.sessions = await this.rest<unknown[]>("GET", "/sessions");
        this.save();
      } catch {
        /* the list arrives with the next change */
      }
      this.emit({ type: "bench:resync" });
    });
    w.on("message", (d) => {
      const ev = JSON.parse(d.toString()) as Record<string, unknown> & { type: string; pi?: string };
      if (ev.pi) return; // session events arrive on the session's own socket
      if (ev.type === "exchange" && ev.row) {
        this.cache.exchanges = [...this.cache.exchanges, ev.row].slice(-KEEP_EXCHANGES);
        this.save();
      }
      this.emit(ev);
    });
    w.on("close", () => {
      this.setUp(false);
      for (const s of this.sockets.values()) s.terminate();
      this.sockets.clear();
      for (const [id, r] of this.waiting) r({ type: "response", id, success: false, error: "the bench connection dropped; reconnecting" });
      this.waiting.clear();
      if (this.closed) return;
      setTimeout(() => this.start(), this.backoff);
      this.backoff = Math.min(this.backoff * 2, 15_000);
    });
    w.on("error", () => undefined); // close follows
  }

  close(): void {
    this.closed = true;
    this.events?.close();
    for (const s of this.sockets.values()) s.close();
  }

  async rest<T = unknown>(method: string, p: string, body?: unknown): Promise<T> {
    const r = await fetch(this.base + p, { method, headers: body === undefined ? {} : { "content-type": "application/json" }, body: body === undefined ? undefined : JSON.stringify(body) });
    const text = await r.text();
    const data = text ? JSON.parse(text) : null;
    if (r.status >= 400) throw new Error((data as { error?: string } | null)?.error ?? `${r.status}`);
    if (method === "GET" && p === "/sessions") this.cache.sessions = data as unknown[];
    return data as T;
  }

  private socket(session: string): Promise<WebSocket> {
    const have = this.sockets.get(session);
    if (have?.readyState === WebSocket.OPEN) return Promise.resolve(have);
    const w = new WebSocket(`${this.base.replace(/^http/, "ws")}/sessions/${encodeURIComponent(session)}/rpc`);
    this.sockets.set(session, w);
    w.on("message", (d) => {
      const ev = JSON.parse(d.toString()) as Record<string, unknown> & { type: string; id?: string };
      const done = ev.type === "response" && ev.id ? this.waiting.get(ev.id) : undefined;
      if (done) {
        this.waiting.delete(ev.id!);
        done(ev);
      }
      this.emit({ ...ev, pi: session });
    });
    w.on("close", () => this.sockets.delete(session));
    w.on("error", () => undefined);
    return new Promise((resolve, reject) => {
      w.once("open", () => resolve(w));
      w.once("close", () => reject(new Error("not connected to the bench; nothing was sent")));
    });
  }

  async rpc(session: string, cmd: Record<string, unknown>): Promise<Record<string, unknown>> {
    if (!this.up) throw new Error("not connected to the bench; nothing was sent");
    const w = await this.socket(session);
    const id = `h${++this.seq}`;
    return new Promise((resolve) => {
      this.waiting.set(id, resolve);
      w.send(JSON.stringify({ ...cmd, id }));
    });
  }

  async messages(session: string): Promise<unknown[]> {
    const have = this.cache.messages[session] ?? [];
    if (!this.up) return have;
    try {
      const r = await this.rest<{ messages: unknown[]; total: number }>("GET", `/sessions/${encodeURIComponent(session)}/messages?after=${have.length}`);
      // A shorter history than the cache means it was cleared or compacted:
      // start over rather than append to a history that no longer exists.
      const all = r.total < have.length ? (await this.rest<{ messages: unknown[] }>("GET", `/sessions/${encodeURIComponent(session)}/messages`)).messages : [...have, ...r.messages];
      this.cache.messages[session] = all.slice(-KEEP_MESSAGES);
      this.save();
      return all;
    } catch {
      return have;
    }
  }
}
```

The cache keeps only the last 200 messages. Once a session outgrows that, `have.length` no longer equals the server's index, so `after=` has to be the true count. Store it beside the messages: `messages: Record<string, { total: number; tail: unknown[] }>`. Page with `after=total` and return `tail` offline. Do it now rather than shipping the bug. Apply the same shape in the test expectations: offline `messages()` still returns 2.

- [ ] **Step 4: Run it and watch it pass**

Run: `cd harness && node --test bench/test/bench-client.test.ts`
Expected: `# pass 1`, `# fail 0`.

- [ ] **Step 5: Cut `main.ts` over**

In `harness/tsconfig.main.json`, replace `"src/pi.ts"` in `include` with `"src/bench-client.ts"`.

In `harness/src/main.ts`:
- Remove `import { Pi, type Fork } from "./pi";`, `const pis = …` and `const pi = new Pi(…)`, together with anything that started `pi` at window creation. Search for `pi.start(` and `pis.set("bench"`.
- Add near the top:
```ts
import { BenchClient } from "./bench-client";

// The bench is remote: HARNESS_BENCH is the local end of the tunnel to this
// person's harness-bench. Without it there is no bench, and the harness says so.
const BENCH = process.env.HARNESS_BENCH;
let bench: BenchClient | undefined;
const toRenderer = (ev: Record<string, unknown>) => {
  if (mainWin && !mainWin.isDestroyed()) mainWin.webContents.send("pi:event", ev);
};
```
- In `app.whenReady().then(...)`, before `createWindow()`:
```ts
  if (BENCH) {
    bench = new BenchClient(BENCH, toRenderer, path.join(app.getPath("userData"), "bench-cache.json"));
    bench.start();
  }
```
- Replace the three handlers `pi`, `pi:spawn` and `pi:stop`, and `app.on("before-quit", …)`, with:
```ts
const needBench = () => {
  if (!bench) throw new Error("no bench: set HARNESS_BENCH to the bench's address");
  return bench;
};
ipcMain.handle("pi", async (_e, cmd: unknown, id: unknown) => {
  if (!cmd || typeof cmd !== "object" || typeof (cmd as { type?: unknown }).type !== "string") throw new Error("a pi command has a type");
  if (typeof id !== "string" || !/^(bench|s-\d+)$/.test(id)) throw new Error("a session id is bench or s-N");
  return needBench().rpc(id, cmd as Record<string, unknown>);
});
// The bench's own surface, method + path allow-listed: the renderer never
// reaches anything else through this.
const BENCH_ROUTES = /^(GET|POST) \/sessions$|^(POST) \/sessions\/(bench|s-\d+)\/(archive|restore|btw)$|^DELETE \/sessions\/(bench|s-\d+)$|^GET \/sessions\/(bench|s-\d+)\/btw$|^GET \/(tasks|procs|healthz)$|^GET \/exchanges\?(session|workspace)=[\w.-]+$|^POST \/import$/;
ipcMain.handle("bench", async (_e, method: unknown, p: unknown, body: unknown) => {
  if (typeof method !== "string" || typeof p !== "string" || !BENCH_ROUTES.test(`${method} ${p}`)) throw new Error(`not a bench route: ${String(method)} ${String(p)}`);
  return needBench().rest(method, p, body);
});
ipcMain.handle("bench:messages", (_e, id: unknown) => {
  if (typeof id !== "string" || !/^(bench|s-\d+)$/.test(id)) throw new Error("a session id is bench or s-N");
  return needBench().messages(id);
});
ipcMain.handle("bench:state", () => ({ configured: !!bench, connected: bench?.connected() ?? false, ...(bench?.cached() ?? { sessions: [], exchanges: [] }) }));
app.on("before-quit", () => bench?.close());
```
- Delete `harness/src/pi.ts`.

- [ ] **Step 6: Cut `preload.ts` over**

In `harness/src/preload.ts`, replace the `spawnPi` and `stopPi` members with:
```ts
  /** The remote bench's own routes (sessions list, archive, delete, btw, exchanges, import). */
  bench: <T = unknown>(method: "GET" | "POST" | "DELETE", path: string, body?: unknown): Promise<T> => ipcRenderer.invoke("bench", method, path, body),
  /** A session's history: the cache first, then whatever the bench has beyond it. */
  benchMessages: (id: string): Promise<unknown[]> => ipcRenderer.invoke("bench:messages", id),
  /** Configured, connected, and the last list and exchanges seen, for a cold offline start. */
  benchState: (): Promise<{ configured: boolean; connected: boolean; sessions: unknown[]; exchanges: unknown[] }> => ipcRenderer.invoke("bench:state"),
```
The `pi` and `onPi` members stay as they are.

- [ ] **Step 7: Typecheck**

Run: `cd harness && npm run build:main`
Expected: exit 0. The renderer typecheck fails until Task 15, on `spawnPi` and `stopPi` in `App.tsx`, which is expected.

- [ ] **Step 8: Commit**

```bash
git add harness/src/bench-client.ts harness/src/main.ts harness/src/preload.ts harness/tsconfig.main.json harness/bench/test/bench-client.test.ts
git rm harness/src/pi.ts
git commit -m "Reach the bench over HARNESS_BENCH instead of spawning pi locally"
```

---

### Task 15: The renderer reads sessions, tasks and processes from the bench

**Files:**
- Modify: `harness/src/renderer/App.tsx` (sessions block ~lines 49–151, `/btw` ~465–497, start-up loop ~441–454, `send` ~514), `harness/src/renderer/live.ts`

**Interfaces:**
```ts
// live.ts additions
export const connected: Accessor<boolean>;          // from {type:"bench"}
export const writable: Accessor<{ ok: boolean; reason?: string }>;
// Task.state gains "lost"; Proc gains pid?: number, lost?: true
// onEvent handles: "bench", "writable", "task" (upsert row), "procs" (replace all rows)
// the "extension_ui_request" harness:procs branch is removed: the bench folds it and sends "procs"
```

No test runner covers the renderer. Verification is the typecheck plus the manual walk-through in Step 6.

- [ ] **Step 1: `live.ts` takes the bench's tables**

In `harness/src/renderer/live.ts`:
- Change the `Task` state union to `"running" | "background" | "done" | "failed" | "cancelled" | "lost"`, and add `pid?: number; lost?: true` to `Proc`.
- Add after `discard`:
```ts
const [connected, setConnected] = createSignal(false);
const [writable, setWritable] = createSignal<{ ok: boolean; reason?: string }>({ ok: true });
export { connected, setConnected, writable };
```
- Replace the module-level `onEvent` with:
```ts
/** Session events carry the session they came from; the bench's own changes carry none. */
export function onEvent(ev: Ev & { pi?: string }) {
  switch (ev.type) {
    case "bench":
      return void setConnected(ev.connected === true);
    case "writable":
      return void setWritable({ ok: ev.ok === true, reason: ev.reason as string | undefined });
    case "procs":
      return void setProcs(produce((ps) => void ps.splice(0, ps.length, ...((ev.rows as Proc[]) ?? []))));
    case "task": {
      // The bench's ledger is the record; the live fold below only adds output.
      const row = ev.row as Task;
      const i = taskIndex(row.id);
      if (i >= 0) setTasks(i, { ...row, output: tasks[i].output });
      else if (row.state === "running" || row.state === "background" || row.state === "lost") setTasks(produce((ts) => void ts.push({ ...row, output: "" })));
      return;
    }
  }
  if (typeof ev.pi === "string" && ev.pi) thread(ev.pi).onEvent(ev);
}
```
- In `makeThread`'s `onEvent`, delete the `case "extension_ui_request"` block.
- Change `stopProc` and `cancel` so a failed send is noted instead of thrown into the void:
```ts
export function stopProc(p: Proc) {
  if (p.session) void window.harness.pi({ type: "prompt", message: `/proc-stop ${p.id}` }, p.session).catch((e: Error) => thread(p.session!).note(e.message));
}
export function cancel(t: Task) {
  const i = taskIndex(t.id);
  if (i >= 0) setTasks(i, { state: "cancelled" });
  void window.harness.pi({ type: "prompt", message: `/cancel ${t.n ? `#${t.n}` : t.id}` }, t.session).catch((e: Error) => thread(t.session).note(e.message));
}
```
- Change the bench alias at the bottom from `thread("bench")` to keep working for imported benches. The exports stay, because other components read `live.messages` and friends.

- [ ] **Step 2: `App.tsx` sessions come from the bench**

Replace lines 49–151, from the `// A bench is several sessions at once` comment through the end of `reallyDelete`, with:
```tsx
  // Sessions are the bench's: the list lives in /bench/sessions.json on the
  // person's bench and every device is a view of it. Nothing here persists the
  // list; the cache for a cold, disconnected start is main's.
  type Session = { id: string; name: string; seq: number; lastActive?: number; archived?: boolean; file?: string };
  const ARCHIVE_AFTER = 24 * 60 * 60 * 1000;
  const [sessions, setSessions] = createStore<Session[]>([]);
  const bench = window.harness.bench;
  const refreshSessions = async () => setSessions(reconcile(await bench<Session[]>("GET", "/sessions")));
  const live_ = () => sessions.filter((x) => !x.archived);
  const archived = () => sessions.filter((x) => x.archived);
  createEffect(() => live.setSessionCount(sessions.length));
  const touch = (_id: string) => undefined; // lastActive is the bench's, set when pi starts a turn
  const sessionThread = (id: string): Thread | undefined => {
    const x = sessions.find((y) => y.id === id);
    return x && { id, name: x.name, kind: "session", readonly: !live.connected(), messages: [], pi: id };
  };
  const fail = (e: Error) => live.thread(cur()).note(e.message);
  const loadThread = async (id: string) => {
    const ms = await window.harness.benchMessages(id);
    live.thread(id).replay(ms);
  };
  const newSession = () =>
    void bench<Session>("POST", "/sessions").then(async (s) => {
      await refreshSessions();
      await loadThread(s.id);
      showThread(s.id);
    }, fail);
  const archiveSession = (id: string) =>
    void bench("POST", `/sessions/${id}/archive`).then(() => {
      sides.filter((t) => t.session === id).forEach((t) => removeSide(t.id));
      if (paneOf(id) >= 0) closeThread(id);
      return refreshSessions();
    }, fail);
  const restoreSession = (id: string) =>
    void bench("POST", `/sessions/${id}/restore`).then(async () => {
      await refreshSessions();
      await loadThread(id);
      showThread(id);
    }, fail);
  const [confirm, setConfirm] = createSignal<{ id: string; items: string[] } | undefined>();
  const idleSessions = () => live_().filter((x) => x.lastActive && Date.now() - x.lastActive > ARCHIVE_AFTER && x.id !== cur());
  /** Delete asks the bench; what it says is in flight comes back as the confirm list. */
  const deleteSession = (id: string) =>
    void bench("DELETE", `/sessions/${id}`, { stop: false }).then(() => afterDelete(id), (e: Error) => {
      if (e.message.includes("in flight: ")) setConfirm({ id, items: e.message.split("in flight: ")[1].split(", ") });
      else fail(e);
    });
  const reallyDelete = (id: string) => {
    setConfirm(undefined);
    void bench("DELETE", `/sessions/${id}`, { stop: true }).then(() => afterDelete(id), fail);
  };
  const afterDelete = (id: string) => {
    sides.filter((t) => t.session === id).forEach((t) => removeSide(t.id));
    live.discard(id);
    if (paneOf(id) >= 0) closeThread(id);
    void refreshSessions().then(() => {
      const next = live_()[0];
      if (next && !panes.some((p) => p.open.length)) showThread(next.id);
    });
  };
```
Then add `reconcile` to the `solid-js/store` import. A thin `inFlight` is gone because the bench answers it. Remove its other uses: search `inFlight(`, and if the Confirm dialog reads it, use `confirm().items` instead. The first-prompt naming (`nameSession`) is the bench's now, so remove `nameSession` and its calls in `send`.

- [ ] **Step 3: Start-up, `/events` and resync**

Replace the block at ~441–454, from `window.harness.onPi(live.onEvent);` through the closing `}` of the `for`, with:
```tsx
  // The bench is live: session events and the bench's own changes land here.
  window.harness.onPi((ev) => {
    live.onEvent(ev);
    if (ev.type === "sessions") void refreshSessions();
    // After a reconnect, every open session pages in what it missed.
    if (ev.type === "bench:resync") void refreshSessions().then(() => Promise.all(live_().map((x) => loadThread(x.id))));
  });
  void window.harness.benchState().then(async (st) => {
    live.setConnected(st.connected);
    if (!st.configured) return void live.thread(first).note("no bench: start the harness with HARNESS_BENCH=http://127.0.0.1:<port>");
    // Cold and offline: the cached list and messages, read-only until connected.
    setSessions(reconcile(st.sessions as Session[]));
    for (const x of live_()) void loadThread(x.id);
    if (st.connected) {
      await refreshSessions();
      void bench<Record<string, unknown>[]>("GET", "/procs").then((rows) => live.onEvent({ type: "procs", rows }));
      void bench<Record<string, unknown>[]>("GET", "/tasks").then((rows) => rows.forEach((row) => live.onEvent({ type: "task", row })));
    }
  });
```
`first` is the id of the pane's first tab. It was `"bench"`, and it must now come from the list. Set `showThread(live_()[0].id)` after `refreshSessions()` whenever no pane has a tab open.

- [ ] **Step 4: `/btw` goes through the bench**

Replace the `/btw` entry's `run` with:
```tsx
      run: (arg) => {
        const from = cur();
        if (!arg.trim()) return L().note("usage: /btw <question> — one question, one answer, nothing changed");
        const id = `btw-${++sideSeq}`;
        setSides((ts) => [...ts, { id, name: `btw · ${arg.slice(0, 40)}`, kind: "btw", readonly: true, messages: [], pi: id, session: from }]);
        if (panes.length < 2) {
          setPanes(produce((ps) => void ps.push({ open: [id], sel: id })));
          setActivePane(panes.length - 1);
        } else showThread(id);
        const side = live.thread(id);
        side.replay([]);
        side.note("a read-only fork of this session on the bench · one answer");
        side.sent(arg);
        side.setStatus("answering");
        // The bench names its own btw id; its streamed events carry that id,
        // so the answer is replayed from the stored entries once it lands.
        void bench<{ id: string; entries: unknown[] }>("POST", `/sessions/${from}/btw`, { question: arg }).then((a) => {
          side.replay(a.entries);
          side.setStatus("answered");
        }, (e: Error) => side.note(e.message));
      },
```
Because the bench names the fork itself, the streamed `btw-N` events can land on a different thread id than the local `sideSeq`. `replay(a.entries)` on completion is the source of truth. `removeSide` no longer stops a process: delete its `window.harness.stopPi(id)` line.

- [ ] **Step 5: Refuse sends when disconnected or unwritable**

In `send`, directly after `if (!c || !pi) return;`, add:
```tsx
    if (!live.connected()) return void live.thread(pi).note("not connected to the bench; nothing was sent");
    if (!live.writable().ok) return void live.thread(pi).note(`the bench cannot save right now (${live.writable().reason}); nothing was sent`);
```
Wherever `window.harness.pi(cmd, pi)` in `send` is `void`ed, add `.catch((e: Error) => L.note(e.message))`. Pass the composer `placeholder={live.connected() ? … : "not connected"}` in the `Chat` usage, wherever the composer's placeholder is set. Search `placeholder=` under `components/Chat.tsx`.

Replace every remaining `window.harness.spawnPi` and `window.harness.stopPi` in `App.tsx`: run `grep -n "spawnPi\|stopPi\|harness.sessions" src/renderer` and expect no matches when done.

- [ ] **Step 6: Build and walk through it**

Run:
```bash
cd harness && npm run typecheck && npm run build
mkdir -p /tmp/bench-ui && node bench/src/main.ts --dir /tmp/bench-ui --host 127.0.0.1 --port 7791 &
HARNESS_BENCH=http://127.0.0.1:7791 npx electron .
```
Expected: typecheck and build exit 0. In the app:
1. **One session.** The sidebar shows `session 1`. A prompt streams an answer, and the session is renamed to the prompt.
2. **Another window.** A second window, via `HARNESS_BENCH=… npx electron . --user-data-dir=/tmp/h2`, shows the same list and streams the same turn live.
3. **`/new` and archive.** `/new`, then archiving `session 1`, updates both windows. `GET http://127.0.0.1:7791/sessions` shows `archived: true`.
4. **Offline.** `kill %1` makes the composer say "not connected". A send notes "nothing was sent". History still shows.
5. **Reconnect.** Restarting `node bench/src/main.ts --dir /tmp/bench-ui --host 127.0.0.1 --port 7791 &` reconnects within 15 s, and the transcript is unchanged.
6. **`/btw`.** `/btw what did I ask?` opens a side pane with one answer. `/tmp/bench-ui/btw/s-1/btw-1.json` exists.

- [ ] **Step 7: Commit**

```bash
git add harness/src/renderer/App.tsx harness/src/renderer/live.ts harness/src/renderer/components/Chat.tsx
git commit -m "Read the harness's sessions, tasks and processes from the bench"
```

---

### Task 16: `bench import` from the laptop

**Files:**
- Modify: `harness/src/main.ts` (a `bench:import` handler), `harness/src/preload.ts`, `harness/src/renderer/App.tsx` (a palette command)

**Interfaces:**
```ts
// preload
benchImport: (sessions: { id: string; name: string; seq: number; lastActive?: number; archived?: boolean }[]) => Promise<{ added: string[]; files: number }>;
// main: reads userData/last-session (bench) and userData/sessions/<id> memos → file paths;
// every *.jsonl in the directory of those files is sent as loose; POST /import
```

- [ ] **Step 1: Main collects the files**

Add to `harness/src/main.ts`:
```ts
// `bench import`: the laptop's sessions onto the bench, once. The list is the
// renderer's localStorage (only it can read it); the files are what the old
// memos point at plus every other session file beside them. The bench merges
// by id and skips file names it has, so running it again changes nothing.
ipcMain.handle("bench:import", async (_e, rows: unknown) => {
  if (!Array.isArray(rows)) throw new Error("import takes the session list");
  const ud = app.getPath("userData");
  const memo = async (id: string) => (await fs.readFile(id === "bench" ? path.join(ud, "last-session") : path.join(ud, "sessions", id), "utf8").catch(() => "")).trim();
  const items = [];
  const dirs = new Set<string>();
  for (const r of rows as { id: string; name: string; seq: number; lastActive?: number; archived?: boolean }[]) {
    if (!/^(bench|s-\d+)$/.test(r.id)) continue;
    const file = await memo(r.id);
    const content = file ? await fs.readFile(file, "utf8").catch(() => undefined) : undefined;
    if (file) dirs.add(path.dirname(file));
    items.push({ row: { id: r.id, name: r.name, seq: r.seq, created: r.lastActive ?? Date.now(), lastActive: r.lastActive ?? Date.now(), archived: !!r.archived }, name: path.basename(file || `${r.id}.jsonl`), content });
  }
  const named = new Set(items.map((i) => i.name));
  const loose = [];
  for (const d of dirs) for (const f of await fs.readdir(d).catch(() => [] as string[])) {
    if (f.endsWith(".jsonl") && !named.has(f)) loose.push({ name: f, content: await fs.readFile(path.join(d, f), "utf8") });
  }
  return needBench().rest("POST", "/import", { items, loose });
});
```
In preload, add:
```ts
  /** Copies this laptop's sessions onto the bench; safe to run again. */
  benchImport: (sessions: unknown[]): Promise<{ added: string[]; files: number }> => ipcRenderer.invoke("bench:import", sessions),
```

- [ ] **Step 2: A palette command marks the local list imported**

In `App.tsx`'s commands array, next to `newSession`, add:
```tsx
    { id: "benchImport", label: "Import this laptop's sessions into the bench", run: () => {
      const raw = localStorage.getItem("harness.sessions");
      if (!raw) return void live.thread(cur()).note("nothing to import: this laptop has no local session list");
      void window.harness.benchImport(JSON.parse(raw) as unknown[]).then((r) => {
        localStorage.setItem("harness.sessions.imported", String(Date.now()));
        live.thread(cur()).note(r.added.length ? `imported ${r.added.length} sessions and ${r.files} files` : "already imported: the bench has every session");
        return refreshSessions();
      }, (e: Error) => live.thread(cur()).note(e.message));
    } },
```

- [ ] **Step 3: Exercise it**

Run on the Mac, whose `userData` holds the old memos:
```bash
cd harness && npm run build
mkdir -p /tmp/bench-imp && node bench/src/main.ts --dir /tmp/bench-imp --host 127.0.0.1 --port 7792 &
HARNESS_BENCH=http://127.0.0.1:7792 npx electron .
```
Then run "Import this laptop's sessions into the bench" from the palette, twice.

Expected:
- **First run:** the note says `imported N sessions and M files`, N matches the old sidebar, and `ls /tmp/bench-imp/sessions` lists the jsonl files.
- **Second run:** the note says `already imported: the bench has every session`.
- **History:** opening an imported session shows its old transcript.

- [ ] **Step 4: Commit**

```bash
git add harness/src/main.ts harness/src/preload.ts harness/src/renderer/App.tsx
git commit -m "Import a laptop's sessions into the bench once"
```

---

### Task 17: Reschedule drill and README

**Files:**
- Modify: `harness/README.md` (the "Running the bench" section)

- [ ] **Step 1: Drill a reschedule locally**

Run:
```bash
cd harness && mkdir -p /tmp/bench-drill
node bench/src/main.ts --dir /tmp/bench-drill --host 127.0.0.1 --port 7793 & P=$!
sleep 3
node -e '
const W=require("ws");const w=new W("ws://127.0.0.1:7793/sessions/s-1/rpc");
w.on("open",()=>w.send(JSON.stringify({id:"1",type:"prompt",message:"Use the process tool to start `sleep 600` named sleeper, then say done."})));
w.on("message",d=>{if(JSON.parse(d).type==="agent_end")process.exit(0)});'
kill -9 $P; pkill -f "sleep 600"
node bench/src/main.ts --dir /tmp/bench-drill --host 127.0.0.1 --port 7793 & P=$!
sleep 3
curl -s http://127.0.0.1:7793/procs; echo; curl -s http://127.0.0.1:7793/sessions/s-1/messages | head -c 300; echo
kill $P
```
Expected:
- **Restart:** the second start takes the lock at once rather than exiting 75, because the killed process's flock child exited with its stdin.
- **Processes:** `/procs` shows `sleeper` with `"lost":true`.
- **History:** `/messages` returns the earlier turn from the reopened session.

- [ ] **Step 2: Document it**

Replace the README's section on running pi locally with:
```markdown
## Running the bench

The harness no longer runs pi. It is a view of a bench: `harness-bench`, one per person per team, serving the sessions in a bench folder on port 7789.

    node bench/src/main.ts --dir /path/to/bench [--host 127.0.0.1] [--port 7789] [--read-only] [--wait] [--idle-secs N]
    node bench/src/main.ts --ping [--port 7789]
    HARNESS_BENCH=http://127.0.0.1:7789 npm start

On the platform the folder is `/bench` and the address is the local end of `kl-connect bench`. There the listener binds `0.0.0.0` behind a gateway-only NetworkPolicy, a held folder lock exits 75 (the agent shows `FolderLocked` and the pod restarts), and `--ping` is the readiness probe. With no client connected and nothing running for the region's `benchIdleSecs`, the bench exits 0 and has no pod; the next connection through `kl-connect bench` starts it, and the first request waits out a cold start of seconds. On a laptop pass `--host 127.0.0.1` and `--wait` to wait for a held lock; leave `--idle-secs` unset (0), and it never sleeps. `--read-only` is a bench whose owner has left the team: it serves history with no pi at all. `npm run bench:test` runs the bench's tests; they need `flock(1)` (`brew install flock` on a Mac).

To bring this laptop's old sessions onto the bench, run "Import this laptop's sessions into the bench" from the palette. Running it twice changes nothing.
```

- [ ] **Step 3: Commit**

```bash
git add harness/README.md
git commit -m "Document running the harness against harness-bench"
```

---

### Task 18: The `workspace-tools` extension

**Files:**
- Create: `harness/pi/workspace-tools.ts`, `harness/bench/test/workspace-tools.test.ts`
- Modify: `harness/pi/kloudlite.ts` (export `call`)

**Interfaces:**
```ts
export const WORKSPACE_TOOLS: string;                        // "read,write,edit,bash,grep,find,ls"
export type IdeCall = { tool: string; args: Record<string, unknown> };
export function toIde(name: string, params: Record<string, any>): IdeCall;
export function fromIde(name: string, status: number, body: any, limit?: number): { content: { type: "text"; text: string }[]; isError: boolean };
export class ToolServer {
  constructor(workspace: string, resolve: (ws: string) => Promise<string>);
  call(c: IdeCall, signal?: AbortSignal): Promise<{ status: number; body: any }>;   // one fresh lookup after a connection error
}
export function resolveFromApi(ws: string): Promise<string>;  // KL_TOOLS_ADDRESS, else GET /v1/workspaces/{ws}/tools?team=$KL_TEAM
export default function (pi: ExtensionAPI): void;             // registers nothing unless KL_TOOLS_WORKSPACE is set
```

The mapping from pi's tools (`dist/core/tools/*.js` in `@mariozechner/pi-coding-agent`) onto the tool server's (`crates/ide/src/tools/{files,exec}.rs`):

| pi tool and parameters | tool server call | answer shown to the model |
|---|---|---|
| `read {path, offset?, limit?}` | `read {path, offset, limit}` | `content` (already line-numbered); a binary file as its size |
| `write {path, content}` | `write {path, content}` | `wrote N bytes to path` |
| `edit {path, edits: [{oldText, newText}]}` | `edit {path, edits: [{old, new}]}` | `applied N edit(s) to path` |
| `bash {command, timeout? (s)}` | `exec {cmd, timeout_ms}`, default 120 s, capped at 600 s | stdout and stderr, `[exit N]` and `isError` when non-zero |
| `grep {pattern, path?, glob?, ignoreCase?, literal?, context?, limit?}` | `grep {pattern (escaped when literal), cwd, glob, ignore_case, context, max}` | `path:line: text` per match |
| `find {pattern, path?, limit?}` | `glob {pattern, cwd}` | paths, cut to `limit` |
| `ls {path?, limit?}` | `exec {cmd: ["ls", "-1Ap", path], head: limit ?? 500}` | the listing |

Every non-2xx answer (404 unknown tool, 400 bad arguments, 403 outside the home, 500 failed) becomes `isError` with only its `error` text. Paths stay as the model wrote them: the tool server resolves a relative path against the workspace dir and confines every path to the home.

- [ ] **Step 1: Write the failing test**

`harness/bench/test/workspace-tools.test.ts`:
```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { toIde, fromIde, ToolServer } from "../../pi/workspace-tools.ts";

test("pi's tools become the tool server's calls", () => {
  assert.deepEqual(toIde("edit", { path: "a.rs", edits: [{ oldText: "x", newText: "y" }] }), { tool: "edit", args: { path: "a.rs", edits: [{ old: "x", new: "y" }] } });
  assert.deepEqual(toIde("bash", { command: "make", timeout: 5 }), { tool: "exec", args: { cmd: "make", timeout_ms: 5000 } });
  assert.equal((toIde("bash", { command: "make", timeout: 3600 }).args as { timeout_ms: number }).timeout_ms, 600_000);
  assert.equal(toIde("grep", { pattern: "a.b", literal: true }).args.pattern, "a\\.b");
  assert.deepEqual(toIde("find", { pattern: "**/*.ts", path: "src" }), { tool: "glob", args: { pattern: "**/*.ts", cwd: "src" } });
  assert.deepEqual(toIde("ls", {}), { tool: "exec", args: { cmd: ["ls", "-1Ap", "."], head: 500 } });
  assert.throws(() => toIde("kl_workspaces", {}), /no workspace tool kl_workspaces/);
});

test("an answer becomes text; a refusal is only its error", () => {
  assert.deepEqual(fromIde("bash", 200, { exit_code: 2, stdout: "out", stderr: "err", timed_out: false }), { content: [{ type: "text", text: "out\nerr\n[exit 2]" }], isError: true });
  assert.deepEqual(fromIde("read", 403, { error: "../x: outside the home" }), { content: [{ type: "text", text: "../x: outside the home" }], isError: true });
  assert.equal(fromIde("grep", 200, { matches: [{ path: "a.rs", line: 3, text: "fn a()" }], truncated: false }).content[0].text, "a.rs:3: fn a()");
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
  await assert.rejects(gone.call(toIde("ls", {})), /workspace api did not answer at 127\.0\.0\.1:1/);
  srv.close();
});
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cd harness && node --test bench/test/workspace-tools.test.ts`
Expected: FAIL with `Cannot find module '.../pi/workspace-tools.ts'`.

- [ ] **Step 3: Export the api call**

In `harness/pi/kloudlite.ts`, change `async function call(method: string, p: string, body?: unknown)` to `export async function call(method: string, p: string, body?: unknown)`. Nothing else in that file changes.

- [ ] **Step 4: Implement**

`harness/pi/workspace-tools.ts`. Node runs the tests with type stripping, so no constructor parameter properties and no enums:
```ts
import { Type } from "typebox";
import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";
import { call } from "./kloudlite.ts";

/**
 * A workspace session's hands. pi runs in the bench pod; these seven tools run
 * in the workspace, as calls to its tool server (`kl ide serve`, crates/ide).
 * The names and parameters are pi's own, so the model sees the tools it knows;
 * the work is the tool server's. The address comes only from /v1, which answers
 * the workspace's owner and nobody else, and is asked again after a connection
 * error because a restarted pod has a new IP. Nothing here is a bench tool:
 * a workspace session starts with `--tools` naming exactly these.
 */
export const WORKSPACE_TOOLS = "read,write,edit,bash,grep,find,ls";
const MAX_EXEC_MS = 600_000;
const escapeRe = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

export type IdeCall = { tool: string; args: Record<string, unknown> };
type Result = { content: { type: "text"; text: string }[]; isError: boolean };
const text = (t: string, isError = false): Result => ({ content: [{ type: "text", text: t || "(no output)" }], isError });

export function toIde(name: string, p: Record<string, any>): IdeCall {
  switch (name) {
    case "read":
      return { tool: "read", args: { path: p.path, offset: p.offset, limit: p.limit } };
    case "write":
      return { tool: "write", args: { path: p.path, content: p.content } };
    case "edit":
      return { tool: "edit", args: { path: p.path, edits: (p.edits ?? []).map((e: { oldText: string; newText: string }) => ({ old: e.oldText, new: e.newText })) } };
    case "bash":
      return { tool: "exec", args: { cmd: p.command, timeout_ms: Math.min(MAX_EXEC_MS, (p.timeout ?? 120) * 1000) } };
    case "grep":
      return { tool: "grep", args: { pattern: p.literal ? escapeRe(p.pattern) : p.pattern, cwd: p.path, glob: p.glob, ignore_case: p.ignoreCase, context: p.context, max: p.limit } };
    case "find":
      return { tool: "glob", args: { pattern: p.pattern, cwd: p.path } };
    case "ls":
      return { tool: "exec", args: { cmd: ["ls", "-1Ap", p.path ?? "."], head: p.limit ?? 500 } };
    default:
      throw new Error(`no workspace tool ${name}`);
  }
}

export function fromIde(name: string, status: number, body: any, limit?: number): Result {
  // The tool server's own refusal is the whole answer: never the body around it.
  if (status >= 400) return text(String(body?.error ?? `the tool server answered ${status}`), true);
  switch (name) {
    case "read":
      return body.binary ? text(`${body.path}: binary, ${body.size} bytes`) : text(body.content + (body.truncated ? `\n[${body.total_lines} lines in all; page with offset]` : ""));
    case "write":
      return text(`wrote ${body.bytes} bytes to ${body.path}`);
    case "edit":
      return text(`applied ${body.applied} edit(s) to ${body.path}`);
    case "bash":
    case "ls": {
      const out = [body.stdout, body.stderr].filter(Boolean).join("\n").trim();
      if (body.timed_out) return text(`${out}\n[timed out]`.trim(), true);
      return body.exit_code === 0 ? text(out) : text(`${out}\n[exit ${body.exit_code}]`.trim(), true);
    }
    case "grep":
      return text((body.matches ?? []).map((m: { path: string; line: number; text: string }) => `${m.path}:${m.line}: ${m.text}`).join("\n") + (body.truncated ? "\n[truncated]" : ""));
    case "find":
      return text((body.paths ?? []).slice(0, limit ?? 1000).join("\n"));
    default:
      return text(JSON.stringify(body));
  }
}

export class ToolServer {
  private workspace: string;
  private resolve: (ws: string) => Promise<string>;
  private address?: string;
  constructor(workspace: string, resolve: (ws: string) => Promise<string>) {
    this.workspace = workspace;
    this.resolve = resolve;
  }
  async call(c: IdeCall, signal?: AbortSignal): Promise<{ status: number; body: any }> {
    for (let attempt = 0; ; attempt++) {
      this.address ??= await this.resolve(this.workspace);
      const at = this.address;
      try {
        const r = await fetch(`http://${at}/tools/${c.tool}`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(c.args), signal });
        return { status: r.status, body: await r.json().catch(() => ({ error: `the tool server answered ${r.status} without JSON` })) };
      } catch (e) {
        if (signal?.aborted) throw e;
        this.address = undefined;
        if (attempt > 0) throw new Error(`workspace ${this.workspace} did not answer at ${at}: ${(e as Error).message}`);
      }
    }
  }
}

export async function resolveFromApi(ws: string): Promise<string> {
  // A laptop points every workspace session at one tool server, such as the local end of `kl-connect ws ide`.
  if (process.env.KL_TOOLS_ADDRESS) return process.env.KL_TOOLS_ADDRESS;
  const team = process.env.KL_TEAM ? `?team=${encodeURIComponent(process.env.KL_TEAM)}` : "";
  const r = await call("GET", `/v1/workspaces/${encodeURIComponent(ws)}/tools${team}`);
  const d = r.data as { address?: string; error?: string } | string | null;
  if (r.status === 200 && d && typeof d === "object" && d.address) return d.address;
  throw new Error(d && typeof d === "object" && d.error ? d.error : `workspace ${ws}: ${typeof d === "string" ? d : r.status}`);
}

export default function (pi: ExtensionAPI) {
  const ws = process.env.KL_TOOLS_WORKSPACE;
  if (!ws) return;
  const server = new ToolServer(ws, resolveFromApi);
  const reg = (name: string, label: string, description: string, parameters: ReturnType<typeof Type.Object>) =>
    pi.registerTool({
      name,
      label,
      description: `${description} Runs in workspace ${ws}.`,
      parameters,
      async execute(_toolCallId, params, signal) {
        const p = params as Record<string, any>;
        try {
          const r = await server.call(toIde(name, p), signal);
          return fromIde(name, r.status, r.body, p.limit);
        } catch (e) {
          return text((e as Error).message, true);
        }
      },
    });
  reg("read", "Read", "Read a text file with line numbers. offset (1-based line) and limit page it.", Type.Object({ path: Type.String(), offset: Type.Optional(Type.Number()), limit: Type.Optional(Type.Number()) }));
  reg("write", "Write", "Create or overwrite a file; parent directories are created.", Type.Object({ path: Type.String(), content: Type.String() }));
  reg("edit", "Edit", "Exact replacements in one file, all or nothing. Each oldText must occur exactly once.", Type.Object({ path: Type.String(), edits: Type.Array(Type.Object({ oldText: Type.String(), newText: Type.String() })) }));
  reg("bash", "Bash", "Run a shell command in the workspace dir and return its output.", Type.Object({ command: Type.String(), timeout: Type.Optional(Type.Number({ description: "Seconds, at most 600" })) }));
  reg("grep", "Grep", "Regex search, gitignore-aware. path is a directory.", Type.Object({ pattern: Type.String(), path: Type.Optional(Type.String()), glob: Type.Optional(Type.String()), ignoreCase: Type.Optional(Type.Boolean()), literal: Type.Optional(Type.Boolean()), context: Type.Optional(Type.Number()), limit: Type.Optional(Type.Number()) }));
  reg("find", "Find", "Files matching a glob, gitignore-aware, newest first.", Type.Object({ pattern: Type.String(), path: Type.Optional(Type.String()), limit: Type.Optional(Type.Number()) }));
  reg("ls", "List", "List a directory; directories end in /.", Type.Object({ path: Type.Optional(Type.String()), limit: Type.Optional(Type.Number()) }));
}
```

- [ ] **Step 5: Run it and watch it pass**

Run: `cd harness && node --test bench/test/workspace-tools.test.ts && npm run typecheck`
Expected: `# pass 3`, `# fail 0`; typecheck exits 0.

- [ ] **Step 6: Commit**

```bash
git add harness/pi/workspace-tools.ts harness/pi/kloudlite.ts harness/bench/test/workspace-tools.test.ts
git commit -m "Run a workspace session's tools on the workspace's tool server"
```

---

### Task 19: Workspace and ephemeral sessions on the bench

**Files:**
- Modify: `harness/bench/src/sessions.ts`, `harness/bench/src/rpc-child.ts`, `harness/bench/src/bench.ts`, `harness/bench/test/fake-pi.ts`
- Create: `harness/bench/test/threads.test.ts`

**Interfaces:**
```ts
// sessions.ts
export type SessionKind = "bench" | "workspace" | "ephemeral";
export type SessionRow = { id: string; name: string; seq: number; file?: string; created: number; lastActive: number; archived: boolean; model?: string; kind?: SessionKind; workspace?: string; target?: string };
// on SessionList: w-{ws} or e-{eph}; an id already listed is returned unchanged
thread(t: { kind: "workspace" | "ephemeral"; workspace: string; eph?: string; target: string; file: string; model?: string }): SessionRow;
// rpc-child.ts
ChildOpts.tools?: string;   // the workspace whose tool server runs this session's tools
// on Bench
openWorkspace(ws: string): Promise<SessionRow>;
openEphemeral(ws: string, eph: string): Promise<SessionRow>;
```

- [ ] **Step 1: Let the fake pi say how it was started**

In `harness/bench/test/fake-pi.ts`, replace:
```ts
    if (cmd.type === "get_state") ok({ sessionFile: file, isStreaming: false });
```
with:
```ts
    if (cmd.type === "get_state") ok({ sessionFile: file, isStreaming: false, argv, tools: process.env.KL_TOOLS_WORKSPACE });
```

- [ ] **Step 2: Write the failing test**

`harness/bench/test/threads.test.ts`:
```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Bench } from "../src/bench.ts";
import { FAKE } from "./rpc-child.test.ts";

const mk = (dir: string) => new Bench({ dir, readOnly: false, model: "fake/m", bin: FAKE });
const tmp = () => fs.mkdtempSync(path.join(os.tmpdir(), "bench-threads-"));
const settle = () => new Promise((r) => setTimeout(r, 150));

test("a workspace thread runs its own pi on the workspace's tools, with its file under workspaces/", async () => {
  const dir = tmp();
  const b = mk(dir);
  await b.start();
  const s = await b.openWorkspace("api");
  assert.equal(s.id, "w-api");
  assert.equal(s.file, path.join(dir, "workspaces", "api", "thread.jsonl"));
  const st = (await b.rpc("w-api", { type: "get_state" })).data as { argv: string[]; tools: string; sessionFile: string };
  assert.equal(st.tools, "api");
  assert.equal(st.sessionFile, s.file);
  assert.equal(st.argv[st.argv.indexOf("--tools") + 1], "read,write,edit,bash,grep,find,ls");
  assert.ok(st.argv.some((x) => x.endsWith("workspace-tools.ts")), "the workspace's tools are loaded");
  assert.ok(!st.argv.some((x) => x.endsWith("background.ts")), "bash stays the workspace's");
  assert.ok(fs.existsSync(s.file!));
  assert.equal((await b.openWorkspace("api")).id, "w-api", "opening twice is one thread");

  const e = await b.openEphemeral("api", "api-eph-1");
  assert.equal(e.id, "e-api-eph-1");
  assert.equal(e.file, path.join(dir, "workspaces", "api", "eph", "api-eph-1.jsonl"));
  assert.equal(e.target, "api-eph-1");
  await assert.rejects(b.openWorkspace("../x"), /not a workspace id/);
  await assert.rejects(b.openEphemeral("api", "a/b"), /not a workspace id/);
  b.stop();
});

test("a thread reopens after a restart and never counts as the bench's open session", async () => {
  const dir = tmp();
  const a = mk(dir);
  await a.start();
  const file = (await a.openWorkspace("api")).file;
  a.stop();
  await settle();
  const b = mk(dir);
  await b.start();
  await settle();
  const st = (await b.rpc("w-api", { type: "get_state" })).data as { sessionFile: string; tools: string };
  assert.equal(st.sessionFile, file);
  assert.equal(st.tools, "api");
  assert.deepEqual(b.sessions.all().filter((x) => (x.kind ?? "bench") === "bench").map((x) => x.id), ["s-1"]);
  await assert.rejects(b.archive("s-1"), /only open session/);
  b.stop();
});
```

- [ ] **Step 3: Run it and watch it fail**

Run: `cd harness && node --test bench/test/threads.test.ts`
Expected: FAIL with `TypeError: b.openWorkspace is not a function`.

- [ ] **Step 4: The row**

In `harness/bench/src/sessions.ts`, replace the `SessionRow` line with:
```ts
export type SessionKind = "bench" | "workspace" | "ephemeral";
/** `kind` absent is a bench session. `target` is the workspace whose tool server runs a thread's tools. */
export type SessionRow = { id: string; name: string; seq: number; file?: string; created: number; lastActive: number; archived: boolean; model?: string; kind?: SessionKind; workspace?: string; target?: string };
```
and add to `SessionList`, after `merge`:
```ts
  thread(t: { kind: "workspace" | "ephemeral"; workspace: string; eph?: string; target: string; file: string; model?: string }): SessionRow {
    const id = t.kind === "workspace" ? `w-${t.workspace}` : `e-${t.eph}`;
    const have = this.get(id);
    if (have) return { ...have };
    const now = Date.now();
    const row: SessionRow = { id, name: t.kind === "workspace" ? t.workspace : `${t.workspace} · ${t.eph}`, seq: 0, file: t.file, created: now, lastActive: now, archived: false, model: t.model, kind: t.kind, workspace: t.workspace, target: t.target };
    this.rows.push(row);
    this.save();
    return { ...row };
  }
```
`seq: 0` keeps a thread out of `create()`'s numbering, which takes the highest seq.

- [ ] **Step 5: The child**

In `harness/bench/src/rpc-child.ts`, add `import { WORKSPACE_TOOLS } from "../../pi/workspace-tools.ts";`, replace the `ChildOpts` line with:
```ts
export type ChildOpts = { dir: string; file?: string; fork?: string; model: string; bin?: string; extDir?: string; cwd?: string; tools?: string };
```
and replace the three lines from `const exts =` through `const child = spawn(` with:
```ts
    // A workspace session loads only the workspace's tools: background.ts would take bash back into the bench pod.
    const exts = o.fork ? [] : o.tools ? ["-e", path.join(extDir, "workspace-tools.ts")] : ["background.ts", "process.ts", "kloudlite.ts"].flatMap((f) => ["-e", path.join(extDir, f)]);
    const args = ["--mode", "rpc", "--model", o.model, "--session-dir", o.dir, ...exts, ...(o.file ? ["--session", o.file] : []), ...(o.fork ? ["--fork", o.fork, "--tools", READ_ONLY_TOOLS] : []), ...(o.tools ? ["--tools", WORKSPACE_TOOLS] : [])];
    const child = spawn(bin, args, { stdio: ["pipe", "pipe", "pipe"], env: o.tools ? { ...process.env, KL_TOOLS_WORKSPACE: o.tools } : process.env, cwd: o.cwd ?? process.env.HOME });
```

- [ ] **Step 6: The Bench**

In `harness/bench/src/bench.ts`:
- Add below the imports:
```ts
// A workspace or ephemeral id becomes a path segment: a DNS label, like the object it names.
const WS_ID = /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/;
const isBench = (s: SessionRow) => (s.kind ?? "bench") === "bench";
```
- In `start()`, replace `if (!this.sessions.all().some((s) => !s.archived)) this.write(() => this.sessions.create(this.opts.model));` with `if (!this.sessions.all().some((s) => !s.archived && isBench(s))) this.write(() => this.sessions.create(this.opts.model));`.
- In `open()`, replace its `const file = …` line and the `c = new RpcChild(…)` line with:
```ts
    const thread = !isBench(s);
    // pi creates a thread's file at the path it is given, so a thread's file need not exist yet.
    const file = thread ? s.file : s.file && fs.existsSync(s.file) ? s.file : undefined;
    const dir = thread ? path.dirname(s.file!) : path.join(this.opts.dir, "sessions");
    c = new RpcChild(s.id, { dir, file, tools: thread ? s.target : undefined, model: s.model ?? this.opts.model, bin: this.opts.bin, extDir: this.opts.extDir }, (ev) => this.fold(s.id, ev));
```
- In `archive()`, replace `this.sessions.all().filter((s) => !s.archived).length < 2` with `isBench(this.sessions.get(id) ?? ({} as SessionRow)) && this.sessions.all().filter((s) => !s.archived && isBench(s)).length < 2`.
- In `remove()`, replace `fs.renameSync(s.file, path.join(trash, path.basename(s.file)));` with `fs.renameSync(s.file, path.join(trash, `${id}-${path.basename(s.file)}`));` (every thread's file is `thread.jsonl`), and replace `if (!this.sessions.all().some((x) => !x.archived)) this.open(this.sessions.create(this.opts.model));` with `if (!this.sessions.all().some((x) => !x.archived && isBench(x))) this.open(this.sessions.create(this.opts.model));`.
- Add after `restore()`:
```ts
  async openWorkspace(ws: string): Promise<SessionRow> {
    return this.openThread("workspace", ws);
  }

  async openEphemeral(ws: string, eph: string): Promise<SessionRow> {
    return this.openThread("ephemeral", ws, eph);
  }

  private openThread(kind: "workspace" | "ephemeral", ws: string, eph?: string): SessionRow {
    for (const x of [ws, eph]) if (x !== undefined && !WS_ID.test(x)) throw new Error(`not a workspace id: ${x}`);
    this.refuse(true);
    const file = kind === "workspace" ? path.join(this.opts.dir, "workspaces", ws, "thread.jsonl") : path.join(this.opts.dir, "workspaces", ws, "eph", `${eph}.jsonl`);
    const s = this.writable.run(() => {
      fs.mkdirSync(path.dirname(file), { recursive: true });
      // An ephemeral is a workspace cut for one agent: its tools run on its own tool server.
      return this.sessions.thread({ kind, workspace: ws, eph, target: eph ?? ws, file, model: this.opts.model });
    });
    if (s.archived) throw new Error(`session ${s.id} is archived; restore it to send`);
    this.open(s);
    this.emit({ type: "sessions" });
    return s;
  }
```

- [ ] **Step 7: Run it and watch it pass**

Run: `cd harness && node --test bench/test/threads.test.ts && npm run bench:test`
Expected: `# pass 2` for the new file; the whole suite `# fail 0`.

- [ ] **Step 8: Exercise with a real pi**

The fake pi writes whatever path it is given; real pi's `--session <path>` goes through `SessionManager.open(path)` (`dist/core/session-manager.js`), which reads entries from a file that may not exist yet. Check that on the real binary rather than assuming it:
```bash
cd harness && rm -rf /tmp/bench-t && mkdir -p /tmp/bench-t
KL_TOOLS_ADDRESS=127.0.0.1:1 node bench/src/main.ts --dir /tmp/bench-t --host 127.0.0.1 --port 7794 & P=$!
sleep 3
curl -s -X POST http://127.0.0.1:7794/workspaces/api/session; echo
node -e '
const W=require("ws");const w=new W("ws://127.0.0.1:7794/sessions/w-api/rpc");
w.on("open",()=>w.send(JSON.stringify({id:"1",type:"prompt",message:"Run ls with the ls tool and tell me what it answered."})));
w.on("message",d=>{const e=JSON.parse(d);if(e.type==="tool_execution_end")console.log(JSON.stringify(e.result));if(e.type==="agent_end")process.exit(0)});'
ls -la /tmp/bench-t/workspaces/api/
kill $P
```
The `curl` uses Task 20's route: run this step once Task 20 is in.

Expected:
- `curl` prints a row with `"id":"w-api"` and `"file":"/tmp/bench-t/workspaces/api/thread.jsonl"`.
- The tool result is an error reading `workspace api did not answer at 127.0.0.1:1: …`, and the model reports it.
- `ls` shows `thread.jsonl` with the turn in it.

If pi refuses a `--session` path that does not exist yet, write pi's session header there first in `openThread` — `{type:"session", version:3, id, timestamp, cwd:"/home/kl"}`, the shape Task 7's test file uses — and note it in the module doc.

- [ ] **Step 9: Commit**

```bash
git add harness/bench/src/sessions.ts harness/bench/src/rpc-child.ts harness/bench/src/bench.ts harness/bench/test/fake-pi.ts harness/bench/test/threads.test.ts
git commit -m "Run workspace and ephemeral sessions on the bench under workspaces/"
```

---

### Task 20: Serve workspace and ephemeral threads

**Files:**
- Modify: `harness/bench/src/server.ts`
- Create: `harness/bench/test/server-threads.test.ts`

| Route | Answer |
|---|---|
| `POST /workspaces/{ws}/session` | `SessionRow` for `w-{ws}` (200, idempotent); 400 when `ws` is not a workspace id; 409 read-only |
| `POST /workspaces/{ws}/eph/{id}/session` | `SessionRow` for `e-{id}` |
| `GET /workspaces/{ws}/messages?after=&limit=` | the thread's `{messages, total}`; `{messages: [], total: 0}` when it was never opened |
| `GET /workspaces/{ws}/eph/{id}/messages?after=&limit=` | the same for an ephemeral |

This replaces Task 11's stand-in, which served the exchange view at `/workspaces/{ws}/messages`. The exchange view stays at `GET /exchanges?workspace=`. The RPC socket is the ordinary `WS /sessions/w-{ws}/rpc`.

- [ ] **Step 1: Write the failing test**

`harness/bench/test/server-threads.test.ts`:
```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import WebSocket from "ws";
import { Bench } from "../src/bench.ts";
import { serve } from "../src/server.ts";
import { FAKE } from "./rpc-child.test.ts";

test("a workspace thread opens over HTTP, streams on its socket and reads back as the workspace's messages", async () => {
  const bench = new Bench({ dir: fs.mkdtempSync(path.join(os.tmpdir(), "bench-srvt-")), readOnly: false, model: "fake/m", bin: FAKE });
  await bench.start();
  const srv = await serve(bench, 0);
  const base = `http://127.0.0.1:${srv.port}`;
  const j = async (method: string, p: string) => {
    const r = await fetch(base + p, { method });
    return { status: r.status, body: await r.json() };
  };

  const opened = await j("POST", "/workspaces/api/session");
  assert.equal(opened.status, 200);
  assert.equal(opened.body.id, "w-api");
  const w = new WebSocket(`ws://127.0.0.1:${srv.port}/sessions/w-api/rpc`);
  await new Promise((r) => w.once("open", r));
  const done = new Promise((r) => w.on("message", (d) => JSON.parse(d.toString()).type === "agent_end" && r(undefined)));
  w.send(JSON.stringify({ id: "1", type: "prompt", message: "hi" }));
  await done;
  assert.equal((await j("GET", "/workspaces/api/messages")).body.total, 2);
  assert.deepEqual((await j("GET", "/workspaces/web/messages")).body, { messages: [], total: 0 });
  assert.equal((await j("POST", "/workspaces/..%2Fx/session")).status, 400);
  assert.equal((await j("POST", "/workspaces/api/eph/api-eph-1/session")).body.id, "e-api-eph-1");
  assert.deepEqual((await j("GET", "/exchanges?workspace=api")).body, [], "the exchange view moved, it did not go");
  w.close();
  bench.stop();
  await srv.close();
});
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cd harness && node --test bench/test/server-threads.test.ts`
Expected: FAIL: `opened.status` is 404 (`no route POST /workspaces/api/session`).

- [ ] **Step 3: Implement**

In `harness/bench/src/server.ts`, replace:
```ts
      if (m === "GET" && p[0] === "workspaces" && p.length === 3 && p[2] === "messages") return send(res, 200, bench.exchanges.byWorkspace(p[1], n("after")));
```
with:
```ts
      if (p[0] === "workspaces") {
        // A thread never opened has no history yet, which is an empty one, not a missing route.
        const thread = (id: string) => (bench.sessions.get(id) ? bench.messages(id, n("after"), n("limit")) : Promise.resolve({ messages: [], total: 0 }));
        if (p.length === 3 && p[2] === "session" && m === "POST") return send(res, 200, await bench.openWorkspace(p[1]));
        if (p.length === 3 && p[2] === "messages" && m === "GET") return send(res, 200, await thread(`w-${p[1]}`));
        if (p.length === 5 && p[2] === "eph" && p[4] === "session" && m === "POST") return send(res, 200, await bench.openEphemeral(p[1], p[3]));
        if (p.length === 5 && p[2] === "eph" && p[4] === "messages" && m === "GET") return send(res, 200, await thread(`e-${p[3]}`));
      }
```
`not a workspace id` falls to 400 through `status()`, and `read-only` to 409, unchanged.

- [ ] **Step 4: Run it and watch it pass**

Run: `cd harness && node --test bench/test/server-threads.test.ts && npm run bench:test`
Expected: `# pass 1`; the whole suite `# fail 0`.

- [ ] **Step 5: Commit**

```bash
git add harness/bench/src/server.ts harness/bench/test/server-threads.test.ts
git commit -m "Serve workspace and ephemeral threads from the bench"
```

---

### Task 21: The renderer opens workspace and ephemeral tabs as live sessions

**Files:**
- Modify: `harness/src/main.ts` (Task 14's handlers), `harness/src/renderer/model.ts` (`threadOf`), `harness/src/renderer/App.tsx` (Task 15's sessions block, `showThread`), `harness/README.md`

No test runner covers the renderer. Verification is the typecheck plus the walk-through in Step 5.

- [ ] **Step 1: Main lets thread ids and routes through**

In `harness/src/main.ts`:
- Add above the `pi` handler: `const SESSION_ID = /^(bench|s-\d+|[we]-[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)$/;`
- In the `pi` and `bench:messages` handlers, replace `!/^(bench|s-\d+)$/.test(id)` with `!SESSION_ID.test(id)`, and their message `"a session id is bench or s-N"` with `"a session id is bench, s-N, w-{workspace} or e-{ephemeral}"`.
- Append to `BENCH_ROUTES`, before its closing `/`: `|^POST \/workspaces\/[a-z0-9-]+(\/eph\/[a-z0-9-]+)?\/session$|^GET \/workspaces\/[a-z0-9-]+(\/eph\/[a-z0-9-]+)?\/messages$`.

- [ ] **Step 2: A workspace or ephemeral thread names its bench session**

In `harness/src/renderer/model.ts`, in `threadOf`, replace:
```ts
    if (w.id === id) return { id, name: w.name, kind: "workspace", readonly: false, messages };
    const e = w.ephemerals.find((x) => x.id === id);
    if (e) return { id, name: e.task, kind: "ephemeral", readonly: true, messages };
```
with:
```ts
    // A workspace's thread and an ephemeral's log are sessions on the bench; their messages are live, not fixtures.
    if (w.id === id) return { id, name: w.name, kind: "workspace", readonly: false, messages: [], pi: `w-${w.id}` };
    const e = w.ephemerals.find((x) => x.id === id);
    if (e) return { id, name: e.task, kind: "ephemeral", readonly: true, messages: [], pi: `e-${e.id}` };
```
`threadsOf` in `App.tsx` already swaps `messages` for `live.thread(t.pi).messages` whenever `pi` is set.

- [ ] **Step 3: Opening a tab opens the session**

In `harness/src/renderer/App.tsx`:
- Add `kind?: "bench" | "workspace" | "ephemeral"` to the `Session` type from Task 15, and make the sidebar lists bench sessions only:
```tsx
  const isBench = (x: Session) => (x.kind ?? "bench") === "bench";
  const live_ = () => sessions.filter((x) => !x.archived && isBench(x));
  const archived = () => sessions.filter((x) => x.archived && isBench(x));
```
- Add after `loadThread`:
```tsx
  // A workspace tab is its session on the bench: open it there (idempotent), then page its history.
  // An ephemeral is watched, never driven, so its tab only reads.
  const openLive = (id: string) => {
    const t = threadOf(machine(), id);
    if (!t?.pi || (t.kind !== "workspace" && t.kind !== "ephemeral")) return;
    const w = machine().workspaces.find((x) => x.id === id || x.ephemerals.some((e) => e.id === id))!;
    const base = t.kind === "workspace" ? `/workspaces/${w.id}` : `/workspaces/${w.id}/eph/${id}`;
    const L = live.thread(t.pi);
    const opened = t.kind === "workspace" && live.connected() && live.writable().ok ? bench("POST", `${base}/session`).catch((e: Error) => L.note(e.message)) : Promise.resolve();
    void opened
      .then(() => bench<{ messages: unknown[] }>("GET", `${base}/messages`))
      .then((r) => L.replay(r.messages), (e: Error) => L.note(e.message));
  };
```
- Call `openLive(id)` as the first line of `showThread` (search `const showThread`). A tab already open replays the same history, which `replay` replaces rather than appends.
- `grep -n "THREADS\[" src/renderer` should now match only the bench thread in `threadOf`.

- [ ] **Step 4: README**

Append to `harness/README.md`'s "Running the bench" section:
```markdown
A workspace tab is a session on the bench too: its pi runs there and its tools run on the workspace's tool server, found through `/v1`. Locally, `KL_TOOLS_ADDRESS=127.0.0.1:<port>` points every workspace session at one tool server, such as the local end of `kl-connect ws ide <workspace>`.
```

- [ ] **Step 5: Build and walk through it**

Run, with a running workspace of yours and its tool server forwarded locally by `kl-connect ws ide <workspace>` (use the local address it reports):
```bash
cd harness && npm run typecheck && npm run build
mkdir -p /tmp/bench-ws && KL_TOOLS_ADDRESS=127.0.0.1:<the forwarded port> node bench/src/main.ts --dir /tmp/bench-ws --host 127.0.0.1 --port 7795 &
HARNESS_BENCH=http://127.0.0.1:7795 npx electron .
```
Expected: typecheck and build exit 0. In the app:
1. **Workspace tab.** Opening a workspace from the sidebar shows an empty thread, and the SESSIONS list does not grow.
2. **Tools run in the workspace.** Asking "run `echo ide-$(id -un)` with bash" shows a Bash call answering `ide-kl`. `/tmp/bench-ws/workspaces/<workspace>/thread.jsonl` holds the turn.
3. **Stopped workspace.** Stopping the forward and asking again shows the call failing with only `workspace <workspace> did not answer at …`; the thread's history is unchanged.
4. **Second window.** A second window on the same bench shows the workspace thread's history when its tab opens.

- [ ] **Step 6: Commit**

```bash
git add harness/src/main.ts harness/src/renderer/model.ts harness/src/renderer/App.tsx harness/README.md
git commit -m "Open workspace and ephemeral tabs as live sessions on the bench"
```

---

## Self-review

- **Spec coverage:**
  - **What runs where:** bench sessions (Tasks 6, 9), workspace and ephemeral sessions in the bench pod with tools on the tool server (Tasks 18–20), `/btw` (Task 10). The address from `/v1` and the fence are the platform plan's Tasks 12 and 13; Task 18 is their client.
  - **Folder layout** (sessions, sessions.json, exchanges.jsonl, btw, tasks.jsonl, procs.json, .lock): Tasks 1–5, 9 and 10. `workspaces/{ws}/thread.jsonl` and `eph/{id}.jsonl`: Task 19.
  - **Surface:** every route in the spec is in Task 11, and the thread routes in Task 20.
  - **Restart-safe:** reopen, lost tasks and procs, and rebuilt exchange views are in Tasks 9 and 12; threads reopen (Task 19). The reschedule drill is Task 17.
  - **Single writer, and the platform's contract:** Tasks 2 and 12 (`0.0.0.0` by default, `--ping`, exit 75 with the holder in the termination message).
  - **Scale to zero:** `Bench.busy` (T9), clients counted and `idleSince` on `/healthz` (T11), `Idle` and the exit 0 naming `idle` (T12). The platform's side — the agent removing a `Succeeded` pod, `/v1` waking it, `kl-connect` waiting — is the platform plan's decision 6.
  - **Departed member:** `--read-only` (T9, T12) is the reader the platform starts for `access: ReadOnly`.
  - **Share unavailable, refuse prompts:** Tasks 8, 9 and 15.
  - **Workspace stopped or unreachable during a tool call:** Task 18 (one fresh lookup, then an error naming the address; `/v1`'s 409 text passed through).
  - **Sync:** fan-out, `/events`, per-device state and the offline cache are in Tasks 11, 14 and 15.
  - **Harness stops doing:** `src/pi.ts`, `localStorage` sessions and memos, local `/btw` and local procs are cut in Tasks 14–15; workspace and ephemeral fixtures in Task 21. The inspector's Queue tab still reads fixtures (gap 2).
  - **Migration:** Task 16.
  - **Unit verification:** append and torn line (T1), atomic replace (T1, T3), both views plus discard (T4), lock contention and exit 75 (T2, T12), tool mapping and refusals (T18), threads across a restart (T19).
- **Placeholders:** none. Three steps say to check vendored behaviour and name the exact change if it differs: T7 Step 4, T14 Step 3, T19 Step 8. T21 Step 5 takes a port only `kl-connect ws ide` can report.
- **Type consistency:**
  - `SessionRow` (T3, extended in T19 with `kind`, `workspace`, `target`) is used by T9, T10, T11, T20 and the renderer's `Session` (T15, T21).
  - `Exchange` (T4) is used by T9 and T11.
  - `TaskRow`/`ProcRow` (T5) mirror the renderer's `Task`/`Proc` plus `lost` (T15).
  - `PiEvent` (T6) is used by T9; `ChildOpts.tools` (T19) carries `SessionRow.target`.
  - `WORKSPACE_TOOLS` is defined once (T18) and imported by `rpc-child.ts` (T19).
  - Thread ids `w-{ws}` and `e-{id}` are minted in `SessionList.thread` (T19) and accepted by `SESSION_ID` (T21).
  - Across plans: port 7789 for the bench, 7788 for the tool server; `GET /v1/workspaces/{id}/tools?team=` answering `{address}` or `{error}` is the platform plan's Task 13; `KL_TEAM` is set on the bench pod by the platform plan's Task 3 and inherited by every child.
  - `BenchClient.rpc` returns the same response shape as the old `Pi.send`, so `live.ts` is unchanged.

## Remaining gaps for the owner

1. **A bench session's message does not yet reach a workspace's thread.** Exchanges are recorded (Task 13), and a workspace's thread is now a live session (Tasks 19–21), but no tool delivers an exchange into the thread as a prompt. That tool, or the bench doing it on `harness:exchange`, is the next step.
2. **The inspector's Queue tab** still reads the fixture `machine.workspaces[].queue`. Pointing it at `GET /exchanges?session=` is a small follow-up, left out because `MachineView`'s `Queue` row shape (`at: string`) differs from `Exchange` (`ts: number`).
3. **Tool calls in flight at a reschedule** become `lost` tasks. pi's own JSONL keeps the dangling tool call, and whether pi resumes cleanly past one is pi's behaviour, untested here.
4. **No background tasks or processes inside a workspace session.** `workspace-tools` runs `bash` as a waiting `exec` (at most 600 s). The tool server's `exec` with `detach`, `process_output` and `/stream/process/{id}` are where `^B` and the process tool go for a workspace.
5. **Nothing starts an ephemeral agent yet.** `POST /workspaces/{ws}/eph/{id}/session` opens its session; the flow that cuts the ephemeral and gives its agent a task is not in either plan, so an ephemeral tab only reads.
