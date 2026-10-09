# TUI State Sync Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Two TUIs on one bench never disagree about a session or the space: the daemon holds every session's state, cards, permission mode, settings, auth and the space, and pushes them; a TUI renders pushes and sends only intents. The bench loses ssh.

**Architecture:** `LocalBackend` (`harness/packages/backend/src/local.ts`) becomes the single owner of session state (`SessionState`), rebuilds, cards (`cards.ts`) and a bench-wide event stream (`watch`). `serve.ts` relays it per connection; `RemoteBackend` exposes it to the TUI. The TUI's sync logic moves out of `app.tsx` into `apps/tui/src/sync.ts`. Rust and the bench image drop sshd; Claude login runs in the daemon over the wire.

**Tech Stack:** Bun + TypeScript (harness workspace, `bun test`), OpenTUI/React TUI, Rust (`bins/kl-connect`, `bins/gateway`, `crates/workspaces`), Docker (`deploy/bench/Dockerfile`).

**Spec:** `docs/superpowers/specs/2026-10-09-tui-state-sync-design.md`

## Global Constraints

- `PROTOCOL` becomes 2 (`wire.ts`). No compatibility shim, no fallback for a protocol-1 bench (dev phase).
- The rule: the daemon holds the state and pushes it; a TUI renders what it was pushed and sends only intents. No TUI sends state it merely remembers.
- Per TUI, never synced: keystrokes, drafts, focus, scroll, view, vim mode, theme, sidebar width.
- Always-allow lives in each TUI process; the daemon never learns of a grant.
- One agent per key; `LocalBackend.session` serialised per key.
- `session_closed` carries `reopen: boolean`: `true` for rebuild or `/clear`, `false` for idle dispose and workspace delete.
- Out of scope: workspace ssh (`kl-connect ws ssh`, `ws ide`, `ssh-config`, gateway workspace tunnel, `authorized_keys` on workspaces), server-side `workspace_wait`, persisting always-allow, cross-bench sync.
- Commits: imperative sentence case, no tool attribution, no trailers.
- Gates: from `harness/`: `bun run check`; `cd harness/packages/backend && bun test`; `cd harness/apps/tui && bun test` (each package separately). Rust: `CARGO_TARGET_DIR=/Volumes/kdisk/target cargo test -p <crate>` and `cargo clippy --workspace --all-targets -- -D warnings`.
- Files stay under ~800 lines; `app.tsx` (1648 today) must shrink, never grow.

## Review Focus

1. A TUI that connects while a turn is blocked on a card shows that card (hello carries pending asks) — pinned in Task 4, test "hello on a late connection returns the pending ask and the mode".
2. A failed `space()` poll never empties the TUI's workspace list or moves focus — pinned in Task 5 ("a failing poll emits the last good view with error") and Task 7 ("a space push with error keeps workspaces and focus").
3. `/model` in TUI A survives TUI B reopening the key — pinned in Task 1 ("B's open does not change A's model").
4. Two opens racing one rebuild build exactly one agent — pinned in Task 2 ("two opens racing a rebuild build one agent").
5. A live event arriving after the open snapshot updates a row instead of duplicating it (snapshot ids equal live ids) — pinned in Task 6 ("snapshot then the same live event renders one row").

---

## File map

| File | Change |
| --- | --- |
| `harness/packages/backend/src/wire.ts` | `PROTOCOL = 2` |
| `harness/packages/backend/src/index.ts` | `SessionState`, `BenchEvent`, `Ask`, `PermMode`, `Hello.asks/mode`, `SessionOpts.initial`, `SessionHandle.state/setCodemode`, `Backend.watch/asks/mode`; drop `cleared?` |
| `harness/packages/backend/src/local.ts` | state, serialised opens, injectable `create`, daemon rebuilds, `#decide`, cards, bench stream, auto-title, `shown` |
| `harness/packages/backend/src/cards.ts` (new) | `Cards`: pending asks, first answer wins, withdraw |
| `harness/packages/backend/src/cards.test.ts` (new) | unit tests for `Cards` |
| `harness/packages/backend/src/spacewatch.ts` (new) | `SpaceWatch`: one poller, dedupe, last-good-with-error |
| `harness/packages/backend/src/spacewatch.test.ts` (new) | unit tests |
| `harness/packages/backend/src/pair.ts` (new) | test helper: `RemoteBackend` over an in-memory `serve()` |
| `harness/packages/backend/src/sync.test.ts` (new) | two-peer tests |
| `harness/packages/backend/src/serve.ts` | `initial`, `state`, `watch`, `ask.answer`, `mode.set`, `setCodemode`; no `permission`/`tool` requests for `question` |
| `harness/packages/backend/src/remote.ts` | mirror of the above; drop `permission` handler; ssh wording |
| `harness/packages/backend/src/clients.ts`, `clients.test.ts` | deleted |
| `harness/packages/backend/src/forget.ts` | notify sessions watchers |
| `harness/packages/backend/src/delegate.ts` | header comment only |
| `harness/packages/backend/src/daemon.ts` | header comment (no ssh) |
| `harness/packages/backend/src/claudelogin.ts` (new) | `claude auth login` under a pty |
| `harness/apps/tui/src/sync.ts` (new) | pure reducers for every push |
| `harness/apps/tui/src/sync.test.tsx` | grows |
| `harness/apps/tui/src/app.tsx` | consumes `sync.ts`; deletes local state |
| `harness/apps/tui/src/remote-args.ts`, `remote.tsx` | drop `--ssh` |
| `harness/packages/agent/src/claude.ts` | `AUTH_MESSAGE` |
| `bins/kl-connect/src/{bench.rs,main.rs,clip.rs}` | ssh path removed |
| `bins/gateway/src/{tunnel.rs,main.rs,resolve.rs}` | `bench_port` removed; bench ticket on `/tunnel/` is 401 |
| `crates/workspaces/src/k8s/bench.rs`, `k8s/tests/bench.rs` | ssh port, netpol port, authorized_keys mount removed |
| `deploy/bench/Dockerfile`, `bench/sshd_config`, `bench/sv/sshd/`, `bench/term/login-shell`, `bench/sv/kl-host/run`, `bench/sessions/main.ts` | sshd removed |

Shared types (Task 1 defines them; every later task uses these exact names):

```ts
// index.ts
export type PermMode = "default" | "acceptEdits" | "plan" | "bypass";
export type SessionState = {
  type: "session_state";
  model: ModelRef;
  thinkingLevel: ThinkingLevel;
  autoCompact: boolean;
  codemode: boolean;
  queued: { steering: string[]; followUp: string[] };
  tokens: number;
};
export type Ask = {
  id: string;
  key: string;                       // the asking key (a delegated session's caller)
  kind: "permission" | "question";
  tool: string;
  title: string;
  subtitle?: string;
  body?: string;
  diff?: string;
  options: { id: string; label: string }[];
};
export type BenchEvent =
  | { type: "ask"; ask: Ask }
  | { type: "ask_resolved"; id: string }
  | { type: "perm"; mode: PermMode }
  | { type: "settings"; settings: Settings }
  | { type: "auth_changed" }
  | { type: "fs_changed"; ws?: string }
  | { type: "space"; view: SpaceView };
```

Ruling recorded here: the spec's `space.watch` and the broadcast `ask` channel are one stream, `Backend.watch(cb: (e: BenchEvent) => void): Promise<() => void>`, so a connection has one subscription. Cost if wrong: splitting it later is a rename in `serve.ts`/`remote.ts`.

---

### Task 1: Protocol 2, pushed session state, opens carry no state

**Files:**
- Modify: `harness/packages/backend/src/wire.ts` (`PROTOCOL`)
- Modify: `harness/packages/backend/src/index.ts` (types above; `SessionOpts`, `SessionHandle`, `SessionEvent`)
- Modify: `harness/packages/backend/src/local.ts` (constructor, `#built`, `baseHandle`, shared-open path 398-418)
- Modify: `harness/packages/backend/src/serve.ts` (`session.open` 19-35, `METHODS` 9-12)
- Modify: `harness/packages/backend/src/remote.ts` (`session()` 41-81)
- Create: `harness/packages/backend/src/pair.ts`
- Create: `harness/packages/backend/src/sync.test.ts`

**Interfaces:**
- Consumes: nothing new.
- Produces:
  - `SessionOpts.initial?: { model?: ModelRef; thinkingLevel?: ThinkingLevel; autoCompact?: boolean; codemode?: boolean }` replacing top-level `model`, `thinkingLevel`, `autoCompact`, `codemode`. `fresh`, `tools`, `permission` stay (Task 4 removes `permission` from the wire).
  - `SessionHandle.state: SessionState` (current snapshot, a getter) and `SessionHandle.setCodemode(on: boolean): Promise<void>` — in Task 1 it updates `state.codemode` and emits `session_state`; Task 2 adds the busy refusal and the rebuild.
  - `SessionEvent` gains `SessionState`.
  - `new LocalBackend({ create?: typeof createSession })`.
  - `pair(backend: Backend): Promise<RemoteBackend>` in `pair.ts`.

- [ ] **Step 1: Write the test helper**

`harness/packages/backend/src/pair.ts`:

```ts
//! Test helper: a RemoteBackend talking to `serve(backend)` over an in-memory wire, so two "TUIs"
//! can share one LocalBackend without a socket. Never imported by product code.
import type { Backend } from "./index";
import { Peer } from "./wire";
import { serve } from "./serve";
import { RemoteBackend } from "./remote";

export async function pair(backend: Backend): Promise<RemoteBackend> {
  const enc = new TextEncoder();
  const nl = (l: string) => enc.encode(l.endsWith("\n") ? l : `${l}\n`);
  let client!: Peer;
  const server = new Peer((l) => client.feed(nl(l)));
  client = new Peer((l) => server.feed(nl(l)));
  serve(backend, server);
  const remote = new RemoteBackend(client);
  // connect() (remote.ts:122) does the same start-up after constructing the backend
  await remote.init();
  return remote;
}
```

If `RemoteBackend` has no `init()`, extract the start-up that `connect()` does after `new RemoteBackend(peer)` (the `hello` request and protocol check at ~150) into `async init(): Promise<void>` and have `connect()` call it. That keeps one start-up path.

- [ ] **Step 2: Write the failing tests**

`harness/packages/backend/src/sync.test.ts`:

```ts
import { test, expect } from "bun:test";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { LocalBackend } from "./local";
import { pair } from "./pair";

async function models() {
  process.env.KLOUDLITE_CONFIG_DIR = mkdtempSync(join(tmpdir(), "kl-cfg-"));
  delete process.env.KL_API_URL;
  const { models } = await import("@kloudlite-tui/agent");
  return models.getModels().filter((x: any) => x.provider !== "anthropic") as any[];
}

test("A's setModel reaches B as session_state; B's open does not change A's model", async () => {
  const ms = await models();
  const [m0, m1] = [ms[0], ms.find((x) => x.id !== ms[0].id) ?? ms[0]];
  const local = new LocalBackend();
  const a = await pair(local), b = await pair(local);
  const ha = await a.session("main", { initial: { model: { provider: m0.provider, id: m0.id } }, fresh: true, tools: [] });
  const hb = await b.session("main", { initial: { model: { provider: m0.provider, id: m0.id } }, tools: [] });
  const seen: any[] = [];
  hb.subscribe((e) => e.type === "session_state" && seen.push(e));
  await ha.setModel({ provider: m1.provider, id: m1.id });
  await Bun.sleep(10);
  expect(seen.at(-1)?.model.id).toBe(m1.id);
  // a third open carrying the old model as `initial` must not move it back
  const hc = await (await pair(local)).session("main", { initial: { model: { provider: m0.provider, id: m0.id } }, tools: [] });
  expect(hc.state.model.id).toBe(m1.id);
  await Promise.all([ha.dispose(), hb.dispose(), hc.dispose()]);
}, 20000);

test("open returns the session state", async () => {
  const [m] = await models();
  const local = new LocalBackend();
  const h = await (await pair(local)).session("w1", { initial: { model: { provider: m.provider, id: m.id }, codemode: false, autoCompact: true, thinkingLevel: "low" }, fresh: true, tools: [] });
  expect(h.state).toMatchObject({ type: "session_state", codemode: false, autoCompact: true, thinkingLevel: "low", tokens: 0, queued: { steering: [], followUp: [] } });
  await h.dispose();
}, 20000);
```

- [ ] **Step 3: Run to verify failure**

Run: `cd harness/packages/backend && bun test src/sync.test.ts`
Expected: FAIL (`pair.ts` imports `init` / `state` undefined / type errors on `initial`).

- [ ] **Step 4: Implement**

1. `wire.ts`: `export const PROTOCOL = 2;`
2. `index.ts`: add the shared types from the File map section. `SessionOpts`: replace `model`, `thinkingLevel`, `autoCompact`, `codemode` with `initial?: {...}`. `SessionHandle`: add `readonly state: SessionState;` and `setCodemode(on: boolean): Promise<void>;`. Add `SessionState` to the `SessionEvent` union.
3. `local.ts`:
   - Constructor: `constructor(o: { create?: typeof createSession } = {}) { this.#create = o.create ?? createSession; }`; replace the direct `createSession(` call at ~433 with `this.#create(`.
   - Add `#state = new Map<string, SessionState>()`. On build: `this.#state.set(key, { type: "session_state", model, thinkingLevel, autoCompact, codemode, queued: { steering: [], followUp: [] }, tokens: 0 })` from `opts.initial ?? {}` falling back to the settings defaults the code uses today.
   - `#emitState(key)`: emit the current state object to the key's live handle subscribers (the same fan-out `baseHandle` uses).
   - In `baseHandle`'s agent subscription: on `queue_update` set `state.queued = { steering: e.steering ?? [], followUp: e.followUp ?? [] }`; on `message_end` with `message.role === "assistant"` add `message.usage?.totalTokens ?? 0` to `state.tokens`; emit after each. Pass the state object in through `hooks` (`hooks.state: SessionState`) so `baseHandle` stays testable with `fakeAgent()`.
   - `setModel`, `setThinkingLevel`, `setAutoCompactionEnabled` on the handle: after the agent call, update the state field and `#emitState`.
   - `setCodemode(on)`: if equal, return; else set `state.codemode = on`, `#emitState` (Task 2 adds the rebuild).
   - Handle `state` getter returns `this.#state.get(key)!`.
   - Shared-open path (398-418): delete the `needsRebuild` model/thinking block (403-412) entirely. Keep only the missing-tool rebuild (`built.tools`).
4. `serve.ts`: `session.open` forwards `initial` unchanged and returns `{ messages, isClaude, busy, state: h.state }`. Add `"setCodemode"` to `METHODS`.
5. `remote.ts`: `session()` sends `initial`, keeps `state` from the reply, updates it on every `session_state` event before passing the event to subscribers, exposes `get state()`, and adds `setCodemode: (on) => this.peer.request("session.call", { key, method: "setCodemode", args: [on] })`.
6. Fix every caller of the old `SessionOpts` fields: `delegate.ts` lines 102 and 139 (`{ initial: { model: o.model, thinkingLevel: o.thinkingLevel, autoCompact: o.autoCompact, codemode: o.codemode }, tools: [] }`), the existing tests in `local.test.ts` (`model:` → `initial: { model: ... }`), and `apps/tui/src/app.tsx` `ensureAgent` (787-796: wrap the four fields in `initial`).

- [ ] **Step 5: Run to verify pass**

Run: `cd harness/packages/backend && bun test` then `cd harness && bun run check`
Expected: all pass; `sync.test.ts` 2 pass.

- [ ] **Step 6: Commit**

```bash
cd /Volumes/kdisk/rustic-git-wt/tui-clear-sync
git add harness/packages/backend/src harness/apps/tui/src/app.tsx
git commit -m "Push session state from the daemon and stop opens from carrying it"
```

---

### Task 2: Rebuilds in the daemon, `session_closed { reopen }`, serialised opens, `/clear` by reopen

**Files:**
- Modify: `harness/packages/backend/src/local.ts` (`session()` 392-471, `sessions.clear` 477, `settings.write` 487, `auth.login` 498, `#cleared` 286-288, `#list` 289, `baseHandle.dispose` 263)
- Modify: `harness/packages/backend/src/index.ts` (`LiveSessionMeta.cleared?` removed; `session_closed` event type)
- Modify: `harness/packages/backend/src/local.test.ts` ("sessions.watch answers at once…" test, lines ~144-171)
- Test: `harness/packages/backend/src/sync.test.ts`

**Interfaces:**
- Consumes: Task 1 `SessionState`, `#state`, `#create`, `setCodemode`.
- Produces:
  - Event `{ type: "session_closed"; reopen: boolean }`.
  - `LocalBackend.#rebuild(key: string): Promise<void>` — disposes the live agent with `reopen: true` and drops `#built[key]`; the next open builds a fresh agent from the stored `SessionState` (model, thinking, autoCompact, codemode), never from the opener's `initial`.
  - `setModel` across the anthropic/pi boundary and `setCodemode` call `#rebuild`; both throw `new Error("a turn is running")` while busy.
  - `#opening = new Map<string, Promise<unknown>>()` per-key chain.

- [ ] **Step 1: Write the failing tests** (append to `sync.test.ts`)

```ts
test("A's /clear: B gets session_closed reopen, and B's reopen is empty", async () => {
  const [m] = await models();
  const local = new LocalBackend();
  const a = await pair(local), b = await pair(local);
  const o = { initial: { model: { provider: m.provider, id: m.id } }, fresh: true, tools: [] };
  const ha = await a.session("main", o);
  const hb = await b.session("main", { ...o, fresh: false });
  const closed: any[] = [];
  hb.subscribe((e) => e.type === "session_closed" && closed.push(e));
  await a.sessions.clear("main");
  await Bun.sleep(10);
  expect(closed).toEqual([{ type: "session_closed", reopen: true }]);
  const again = await b.session("main", { tools: [] });
  expect(again.messages).toEqual([]);
  await Promise.all([ha.dispose(), again.dispose()]);
}, 20000);

test("idle dispose says reopen false", async () => {
  const [m] = await models();
  const local = new LocalBackend();
  const h = await (await pair(local)).session("w2", { initial: { model: { provider: m.provider, id: m.id } }, fresh: true, tools: [] });
  const closed: any[] = [];
  h.subscribe((e) => e.type === "session_closed" && closed.push(e));
  await h.dispose();
  expect(closed).toEqual([{ type: "session_closed", reopen: false }]);
}, 20000);

test("two opens racing a rebuild build one agent", async () => {
  const [m] = await models();
  const { createSession } = await import("@kloudlite-tui/agent");
  let builds = 0;
  const local = new LocalBackend({ create: ((o: any) => (builds++, createSession(o))) as any });
  const o = { initial: { model: { provider: m.provider, id: m.id } }, fresh: true, tools: [] };
  const h = await local.session("main", o);
  expect(builds).toBe(1);
  const toggle = h.setCodemode(!h.state.codemode); // rebuild
  const [x, y] = await Promise.all([local.session("main", { tools: [] }), local.session("main", { tools: [] })]);
  await toggle;
  expect(builds).toBe(2);
  await Promise.all([x.dispose(), y.dispose()]);
}, 20000);

test("setCodemode while busy is refused", async () => {
  const [m] = await models();
  const local = new LocalBackend();
  const h = await local.session("w3", { initial: { model: { provider: m.provider, id: m.id } }, fresh: true, tools: [] });
  (local as any).busyForTest("w3", true);
  await expect(h.setCodemode(!h.state.codemode)).rejects.toThrow("a turn is running");
  (local as any).busyForTest("w3", false);
  await h.dispose();
}, 20000);
```

Add to `LocalBackend` a test seam used only by that test:

```ts
  /** test seam: mark a key busy without a model turn */
  busyForTest(key: string, on: boolean) { on ? this.#busy.add(key) : this.#busy.delete(key); }
```

(`createSession` is the name `local.ts` imports today; if the agent package exports it under another name, use that name in both the import above and `#create`.)

- [ ] **Step 2: Run to verify failure**

Run: `cd harness/packages/backend && bun test src/sync.test.ts`
Expected: FAIL (`session_closed` lacks `reopen`; clear emits nothing; builds count wrong).

- [ ] **Step 3: Implement**

1. `baseHandle` (225-279): `dispose(reopen = false)` emits `{ type: "session_closed", reopen }`. The handle's public `dispose()` keeps no argument (`reopen: false`); the backend calls an internal `close(reopen)` the hooks expose.
2. Serialise `session()`:

```ts
  #opening = new Map<string, Promise<unknown>>();
  session(key: string, opts: SessionOpts): Promise<SessionHandle> {
    // one open at a time per key: an open racing a rebuild must find the rebuilt agent, never
    // build a second one on the same session file (one writer per file)
    const prev = this.#opening.get(key) ?? Promise.resolve();
    const next = prev.catch(() => {}).then(() => this.#open(key, opts));
    this.#opening.set(key, next);
    void next.finally(() => this.#opening.get(key) === next && this.#opening.delete(key)).catch(() => {});
    return next;
  }
```

   Rename today's `session()` body to `async #open(key, opts)`.
3. `#rebuild(key)`:

```ts
  /** Dispose the live agent so every view reopens it; the next open rebuilds from #state. */
  async #rebuild(key: string): Promise<void> {
    await (this.#opening.get(key) ?? Promise.resolve()).catch(() => {});
    if (this.#busy.has(key)) throw new Error("a turn is running");
    const live = this.#live.get(key);
    this.#built.delete(key);
    await live?.close(true);
  }
```

   `#open` builds from `this.#state.get(key)` when present (model, thinkingLevel, autoCompact, codemode), else from `opts.initial`, else from settings defaults.
4. `setCodemode(on)`: refuse when busy (`throw new Error("a turn is running")`), set state, then `await this.#rebuild(key)`.
5. `setModel(ref)`: when `(ref.provider === "anthropic") !== isClaude` → refuse when busy, set `state.model = ref`, `#rebuild(key)`; else today's `setModel` plus state update.
6. `sessions.clear(key)` (477): archive history as today, set `this.#state.get(key)` tokens to 0 and queued to empty, then `#rebuild(key)` (skip the busy refusal for clear: abort first if busy, as `/clear` did in the TUI). Delete `#cleared` and the `cleared` field in `#list` and `LiveSessionMeta`.
7. `settings.write(s)` (487): when `s.codemode` is present, for every live key: idle → set `state.codemode`, `#rebuild`; busy → add to the existing `#rebuild` set (rename that set to `#rebuildAtEnd` since `#rebuild` is now the method) so `agent_end` rebuilds it.
8. `auth.login` (498): after success, `#rebuild` every idle live key whose `state.model.provider === "anthropic"`.
9. `local.test.ts` "sessions.watch answers at once…": delete the two `cleared` assertions; after `await b.sessions.clear("main")` assert `lists.length` grew (`toBeGreaterThan(n + 1)`) since the rebuild closes and lists change.

- [ ] **Step 4: Run to verify pass**

Run: `cd harness/packages/backend && bun test` then `cd harness && bun run check`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add harness/packages/backend/src harness/apps/tui/src
git commit -m "Rebuild sessions in the daemon and tell every view to reopen"
```

---

### Task 3: User messages carry `shown`; auto-title in the daemon

**Files:**
- Modify: `harness/packages/backend/src/local.ts` (`baseHandle` subscription and `prompt` at 254; `#attach` 369-390; `messages` snapshot)
- Test: `harness/packages/backend/src/local.test.ts`

**Interfaces:**
- Consumes: `roleCard(key)` (local.ts), `baseHandle`, `fakeAgent()` (local.test.ts).
- Produces:
  - `message_start` events with `message.role === "user"` carry `shown: string` — the user text with a leading `${roleCard(key)}\n\n` removed.
  - `stripCard(key: string, text: string): string` exported from `local.ts`.
  - `SessionHandle.messages` returns user messages whose first text block has the card stripped.
  - A client-typed `prompt` to a key with no name names it `text.replace(/\s+/g, " ").slice(0, 40)`.

- [ ] **Step 1: Write the failing tests** (append to `local.test.ts`)

```ts
test("a user message_start carries shown without the role card", () => {
  const agent = fakeAgent();
  const h = baseHandle(agent, "ws-a", { busy: new Set(), onEnd() {}, onDispose() {}, state: undefined as any });
  const got: any[] = [];
  h.subscribe((e) => got.push(e));
  const card = roleCard("ws-a");
  agent.emit({ type: "message_start", message: { role: "user", timestamp: 7, content: [{ type: "text", text: `${card}\n\nhello` }] } });
  expect(got[0].shown).toBe("hello");
  agent.emit({ type: "message_start", message: { role: "user", timestamp: 8, content: [{ type: "text", text: "again" }] } });
  expect(got[1].shown).toBe("again");
});

test("stripCard leaves text without a card alone", () => {
  expect(stripCard("main", "plain")).toBe("plain");
  expect(stripCard("main", `${roleCard("main")}\n\nx`)).toBe("x");
});

test("a typed prompt names an unnamed session", async () => {
  process.env.KLOUDLITE_CONFIG_DIR = mkdtempSync(join(tmpdir(), "kl-cfg-"));
  delete process.env.KL_API_URL;
  const { models } = await import("@kloudlite-tui/agent");
  const m = models.getModels().find((x: any) => x.provider !== "anthropic") as any;
  const b = new LocalBackend();
  const h = await b.session("w7", { initial: { model: { provider: m.provider, id: m.id } }, fresh: true, tools: [], client: true } as any);
  await h.prompt("  fix   the build please, it fails on the linker step every time  ").catch(() => {});
  const meta = (await b.sessions.list()).find((s) => s.key === "w7");
  expect(meta?.name).toBe("fix the build please, it fails on the li");
  await h.dispose();
}, 20000);
```

Add `stripCard` to the test file's import from `./local`.

- [ ] **Step 2: Run to verify failure**

Run: `cd harness/packages/backend && bun test src/local.test.ts`
Expected: FAIL (`shown` undefined; `stripCard` not exported; name undefined).

- [ ] **Step 3: Implement**

```ts
/** The person's own words: the first prompt carries the role card, which no view should show. */
export function stripCard(key: string, text: string): string {
  const card = `${roleCard(key)}\n\n`;
  return text.startsWith(card) ? text.slice(card.length) : text;
}
```

In `baseHandle`'s agent subscription, before fan-out:

```ts
if (e.type === "message_start" && e.message?.role === "user") {
  const text = (e.message.content ?? []).filter((b: any) => b.type === "text").map((b: any) => b.text).join("\n");
  e = { ...e, shown: stripCard(key, text) };
}
```

`messages` getter: map user messages, replacing the first text block's text with `stripCard(key, text)` (copy, never mutate the agent's array).

In `#attach`'s typed `prompt` (client path): before delegating, `if (!this.#meta(key)?.name) await this.sessions.name(key, text.replace(/\s+/g, " ").trim().slice(0, 40))` using whatever `local.ts` uses to read a key's meta for `#list`.

- [ ] **Step 4: Run to verify pass**

Run: `cd harness/packages/backend && bun test`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add harness/packages/backend/src
git commit -m "Show user messages from events and title sessions in the daemon"
```

---

### Task 4: Cards, permission mode and the bench stream in the daemon

**Files:**
- Create: `harness/packages/backend/src/cards.ts`, `harness/packages/backend/src/cards.test.ts`
- Modify: `harness/packages/backend/src/local.ts` (`installGate` call at 444, `#deps` 342-351, `routed` 421-428, `#offs` 309, `#clients` 307, `#attach` client registration, `hello` 323-336)
- Modify: `harness/packages/backend/src/index.ts` (`Hello.asks`, `Hello.mode`, `Backend.watch`, `Backend.asks`, `Backend.mode`)
- Modify: `harness/packages/backend/src/serve.ts`, `harness/packages/backend/src/remote.ts`
- Modify: `harness/packages/backend/src/delegate.ts` (header line 5 only)
- Delete: `harness/packages/backend/src/clients.ts`, `harness/packages/backend/src/clients.test.ts`
- Test: `harness/packages/backend/src/sync.test.ts`, `local.test.ts` (`registryFor` test ~349)

**Interfaces:**
- Consumes: `BenchEvent`, `Ask`, `PermMode` (Task 1 types); `installGate` unchanged.
- Produces:
  - `class Cards { constructor(emit: (e: BenchEvent) => void); ask(a: Omit<Ask, "id">, signal: AbortSignal, fallback: string): Promise<string>; answer(id: string, choice: string): void; withdrawKey(key: string): void; pending(): Ask[] }`
  - `LocalBackend.permit(key: string, req: PermissionRequest, signal: AbortSignal): Promise<Decision>` (mode check then card; used as default permission and as `deps.permit`).
  - `Backend.watch(cb: (e: BenchEvent) => void): Promise<() => void>`
  - `Backend.asks.answer(id: string, choice: string): Promise<void>`
  - `Backend.mode.set(m: PermMode): Promise<void>` (emits `perm`)
  - `Hello.asks: Ask[]`, `Hello.mode: PermMode`
  - wire methods `watch` (stream on channel `"bench"`, id `"*"`), `ask.answer`, `mode.set`.

- [ ] **Step 1: Write the failing unit tests**

`harness/packages/backend/src/cards.test.ts`:

```ts
import { test, expect } from "bun:test";
import { Cards } from "./cards";

const base = { key: "main", kind: "permission" as const, tool: "bash", title: "Permission required", options: [{ id: "once", label: "Allow once" }, { id: "reject", label: "Reject" }] };

test("first answer wins, every connection hears ask and ask_resolved, a late answer is ignored", async () => {
  const seen: any[] = [];
  const c = new Cards((e) => seen.push(e));
  const p = c.ask(base, new AbortController().signal, "reject");
  const id = seen[0].ask.id;
  expect(seen[0].type).toBe("ask");
  expect(c.pending().map((a) => a.id)).toEqual([id]);
  c.answer(id, "once");
  c.answer(id, "reject");
  expect(await p).toBe("once");
  expect(seen.filter((e) => e.type === "ask_resolved")).toEqual([{ type: "ask_resolved", id }]);
  expect(c.pending()).toEqual([]);
});

test("abort withdraws with the fallback and resolves the card", async () => {
  const seen: any[] = [];
  const c = new Cards((e) => seen.push(e));
  const ac = new AbortController();
  const p = c.ask(base, ac.signal, "reject");
  ac.abort();
  expect(await p).toBe("reject");
  expect(seen.at(-1)).toEqual({ type: "ask_resolved", id: seen[0].ask.id });
});

test("withdrawKey resolves every ask of that key only", async () => {
  const c = new Cards(() => {});
  const a = c.ask(base, new AbortController().signal, "reject");
  const b = c.ask({ ...base, key: "ws-1" }, new AbortController().signal, "reject");
  c.withdrawKey("main");
  expect(await a).toBe("reject");
  expect(c.pending().map((x) => x.key)).toEqual(["ws-1"]);
  c.withdrawKey("ws-1");
  await b;
});
```

- [ ] **Step 2: Run to verify failure**

Run: `cd harness/packages/backend && bun test src/cards.test.ts`
Expected: FAIL (`./cards` not found).

- [ ] **Step 3: Implement `cards.ts`**

```ts
//! Cards: the daemon's pending questions to the person (permission and `question` asks).
//!
//! Every connection hears `ask` and `ask_resolved`; each TUI shows a card only for its active key.
//! First answer wins; a late answer is ignored. An ask ends one of three ways: answered, its
//! signal aborted (turn interrupted), or its key withdrawn (agent disposed, workspace deleted) —
//! the last two resolve with the caller's fallback (`reject` for a permission).
import type { Ask, BenchEvent } from "./index";

type Pending = { ask: Ask; resolve: (choice: string) => void };

export class Cards {
  #pending = new Map<string, Pending>();
  #n = 0;
  constructor(private readonly emit: (e: BenchEvent) => void) {}

  ask(a: Omit<Ask, "id">, signal: AbortSignal, fallback: string): Promise<string> {
    const ask: Ask = { ...a, id: `a${Date.now().toString(36)}${(++this.#n).toString(36)}` };
    return new Promise((resolve) => {
      const done = (choice: string) => {
        if (!this.#pending.delete(ask.id)) return;
        signal.removeEventListener("abort", onAbort);
        this.emit({ type: "ask_resolved", id: ask.id });
        resolve(choice);
      };
      const onAbort = () => done(fallback);
      this.#pending.set(ask.id, { ask, resolve: done });
      if (signal.aborted) return onAbort();
      signal.addEventListener("abort", onAbort);
      this.emit({ type: "ask", ask });
    });
  }

  answer(id: string, choice: string): void {
    this.#pending.get(id)?.resolve(choice);
  }

  /** Withdraw every ask of a key: its agent is gone, nobody waits on the answer. */
  withdrawKey(key: string, fallback = "reject"): void {
    for (const p of [...this.#pending.values()]) if (p.ask.key === key) p.resolve(fallback);
  }

  pending(): Ask[] {
    return [...this.#pending.values()].map((p) => p.ask);
  }
}
```

- [ ] **Step 4: Run unit tests**

Run: `cd harness/packages/backend && bun test src/cards.test.ts`
Expected: 3 pass.

- [ ] **Step 5: Write the failing two-peer tests** (append to `sync.test.ts`)

```ts
const bashReq = { name: "bash", args: { command: "ls" } } as any;

test("a gated call asks both TUIs; B answers; A hears ask_resolved; A's late answer is ignored", async () => {
  const local = new LocalBackend();
  const a = await pair(local), b = await pair(local);
  const ea: any[] = [], eb: any[] = [];
  await a.watch((e) => ea.push(e));
  await b.watch((e) => eb.push(e));
  const decision = local.permit("main", bashReq, new AbortController().signal);
  await Bun.sleep(10);
  const ask = eb.find((e) => e.type === "ask")!.ask;
  expect(ea.find((e) => e.type === "ask")?.ask.id).toBe(ask.id);
  await b.asks.answer(ask.id, "reject");
  expect(await decision).toEqual({ block: true, reason: "The user rejected this tool call." });
  await Bun.sleep(10);
  expect(ea).toContainEqual({ type: "ask_resolved", id: ask.id });
  await a.asks.answer(ask.id, "once"); // ignored, no throw
});

test("aborting the turn resolves the ask as reject", async () => {
  const local = new LocalBackend();
  const ac = new AbortController();
  const d = local.permit("main", bashReq, ac.signal);
  ac.abort();
  expect((await d).block).toBe(true);
});

test("hello on a late connection returns the pending ask and the mode", async () => {
  const local = new LocalBackend();
  const a = await pair(local);
  await a.mode.set("acceptEdits");
  const d = local.permit("main", bashReq, new AbortController().signal);
  const c = await pair(local);
  expect(c.hello().asks.map((x) => x.tool)).toEqual(["bash"]);
  expect(c.hello().mode).toBe("acceptEdits");
  await c.asks.answer(c.hello().asks[0]!.id, "once");
  expect(await d).toEqual({});
});

test("mode: plan answers without a card, bypass allows, acceptEdits allows edits only", async () => {
  const local = new LocalBackend();
  const a = await pair(local);
  const seen: any[] = [];
  await a.watch((e) => seen.push(e));
  await a.mode.set("plan");
  expect((await local.permit("main", bashReq, new AbortController().signal)).block).toBe(true);
  await a.mode.set("bypass");
  expect(await local.permit("main", bashReq, new AbortController().signal)).toEqual({});
  await a.mode.set("acceptEdits");
  expect(await local.permit("main", { name: "edit", args: { path: "x" } } as any, new AbortController().signal)).toEqual({});
  expect(seen.filter((e) => e.type === "perm").map((e) => e.mode)).toEqual(["plan", "bypass", "acceptEdits"]);
  expect(seen.some((e) => e.type === "ask")).toBe(false);
});
```

- [ ] **Step 6: Implement the daemon side**

1. `local.ts` fields: `#mode: PermMode = "default"` (per daemon process; resets on restart, as the TUI's did), `#bench = new Set<(e: BenchEvent) => void>()`, `#cards = new Cards((e) => this.#broadcast(e))`, `#broadcast(e) { for (const f of this.#bench) f(e); }`.
2. `permit` — the `gate` logic moved from `app.tsx:920-976`, minus always-allow:

```ts
  /** Tools that only mutate the workspace's files — what acceptEdits waves through. */
  static readonly EDITS = new Set(["write", "edit", "patch"]);

  async permit(key: string, { name, args, diff, session, reason, claimed }: PermissionRequest, signal: AbortSignal): Promise<Decision> {
    // a delegated session asks through its caller: the card belongs to the caller's key
    const asker = session ?? key;
    const mode = this.#mode;
    // plan mode answers rather than asks: a refusal the model can read beats a card every turn
    if (mode === "plan")
      return { block: true, reason: `Plan mode: ${name} is not available. Research and explain what you would do; the user will leave plan mode when they want it done.` };
    if (mode === "bypass" || (mode === "acceptEdits" && LocalBackend.EDITS.has(name))) return {};
    const why = reason ? `Why: ${reason}` : claimed ? `Says you asked: “${claimed}”, which is not in your messages this turn` : "No reason given";
    const subtitle = name === "bash" ? "Shell command" : name === "web_fetch" ? "Fetch a URL" : LocalBackend.EDITS.has(name) ? `${name === "write" ? "Write" : "Edit"} ${args?.path ?? "file"}` : `Run ${name}`;
    const detail = name === "bash" ? `$ ${args?.command ?? ""}` : name === "web_fetch" ? String(args?.url ?? "") : diff ? undefined : JSON.stringify(args ?? {}).slice(0, 400);
    const choice = await this.#cards.ask({
      key: asker, kind: "permission", tool: name, title: "Permission required", subtitle,
      body: [why, detail].filter(Boolean).join("\n\n"), diff,
      options: [{ id: "once", label: "Allow once" }, { id: "always", label: "Allow always" }, { id: "reject", label: "Reject" }],
    }, signal, "reject");
    return choice === "reject" ? { block: true, reason: "The user rejected this tool call." } : {};
  }
```

   ("always" answers like "once" in the daemon: the grant is the TUI's, spec §8.)
3. Build path (~444): `installGate(agent, opts.permission ?? ((req, s) => this.permit(key, req, s)), …)` — replace the `#clients.route` argument. `#deps().permit` (349) becomes `(k, req, s) => this.permit(k, req, s)`.
4. `question` tool in the daemon registry (the `registryFor` registry). Same schema the TUI registers today (`app.tsx:856-873`):

```ts
const question = (key: string, cards: Cards): Tool => ({
  name: "question",
  description: "Ask the user a question and wait for their answer. Use it when you need a decision or clarification. Give 2-5 short answer options.",
  inputSchema: { type: "object", properties: { question: { type: "string", description: "The question to ask." }, options: { type: "array", items: { type: "string" }, description: "The answer options that the user can select." } }, required: ["question", "options"] },
  run: async ({ question, options }: { question: string; options: string[] }, signal?: AbortSignal) => {
    const picked = await cards.ask({ key, kind: "question", tool: "question", title: question, options: options.map((label, i) => ({ id: String(i), label })) }, signal ?? new AbortController().signal, "__withdrawn");
    if (picked === "__withdrawn") throw new Error("the question was withdrawn (turn interrupted)");
    return options[Number(picked)] ?? picked;
  },
});
```

   Use the `Tool` type and `run` signature `registryFor` uses for its other tools; if `run` gets no signal, pass a fresh controller's signal (dispose withdraws via `withdrawKey`).
5. `onDispose` (449-459): `this.#cards.withdrawKey(key)`. Workspace delete path (`forgetSessions` caller): `withdrawKey` for every key of the workspace.
6. Delete `routed` (421-428), `#offs`, `#clients`, the client registration in `#attach` (keep `#attach`'s typed prompt/steer/followUp), the `Clients` import (27), `clients.ts`, `clients.test.ts`.
7. `hello()` adds `asks: this.#cards.pending(), mode: this.#mode`.
8. Backend methods: `watch(cb)` adds to `#bench`, returns `async () => off`; `asks = { answer: async (id, c) => this.#cards.answer(id, c) }`; `mode = { set: async (m) => { this.#mode = m; this.#broadcast({ type: "perm", mode: m }); } }`.
9. `serve.ts`: `peer.handle("watch", async () => { const off = await backend.watch((e) => peer.emit("bench", "*", e)); offs.push(off); })` (use the same disposal list `sessions.watch` uses, 59-64 and 79-85); `peer.handle("ask.answer", ({ id, choice }) => backend.asks.answer(id, choice))`; `peer.handle("mode.set", ({ mode }) => backend.mode.set(mode))`. `session.open`: drop the `permission` request to the client and the `tool` routing for `question`; the client sends no `permission` now (`opts.permission` is only for in-process callers such as delegate).
10. `remote.ts`: delete the `permission` handler (15-29 keeps `tool` and `auth.prompt`); add `watch(cb)` (request `watch`, route `bench` channel events in `onEvent` 30-34 to the callbacks), `asks.answer`, `mode.set`. `session()` stops sending `permission`.
11. `delegate.ts` line 5: "Delegated sessions ask through `deps.permit`, the daemon's gate: the card is raised for the caller's key on every connected TUI."
12. `local.test.ts` `registryFor` test (~349): the deps object has no client routing; `question` comes from the registry, not `opts.tools` — assert `registry.has("question")` (or the equivalent lookup that test already uses for other tool names).

- [ ] **Step 7: Run to verify pass**

Run: `cd harness/packages/backend && bun test` then `cd harness && bun run check`
Expected: all pass. (`bun run check` will flag the TUI's use of removed `permission`/`question` wiring; fix the compile errors in `app.tsx` minimally — drop `permission:` from `ensureAgent`'s open and the `tuiTools.push` of `question` — Task 7 does the real TUI work.)

- [ ] **Step 8: Commit**

```bash
git add -A harness/packages/backend/src harness/apps/tui/src/app.tsx
git commit -m "Raise permission and question cards from the daemon to every TUI"
```

---

### Task 5: Settings, auth, files and space on the bench stream

**Files:**
- Create: `harness/packages/backend/src/spacewatch.ts`, `harness/packages/backend/src/spacewatch.test.ts`
- Modify: `harness/packages/backend/src/local.ts` (`settings.write` 487, `auth.login` 498, `baseHandle` subscription for `tool_execution_end`/`agent_end`, `watch`)
- Modify: `harness/packages/backend/src/forget.ts`
- Test: `harness/packages/backend/src/sync.test.ts`

**Interfaces:**
- Consumes: Task 4 `#broadcast`, `#bench`, `watch`.
- Produces:
  - `class SpaceWatch { constructor(read: () => Promise<SpaceView>, emit: (v: SpaceView) => void, everyMs = 5000); start(): void; stop(): void; poke(): void; last(): SpaceView | undefined }`
  - `forgetSessions(ws, live, changed: () => void)` — third parameter called after the sessions are forgotten.
  - Bench events `settings`, `auth_changed`, `fs_changed { ws? }`, `space { view }`.

- [ ] **Step 1: Write the failing unit tests**

`harness/packages/backend/src/spacewatch.test.ts`:

```ts
import { test, expect } from "bun:test";
import { SpaceWatch } from "./spacewatch";

const view = (n: number) => ({ available: true, user: "u", workspaces: [{ id: `w${n}` }], environments: [] }) as any;

test("emits on change only; a failing poll emits the last good view with error", async () => {
  let next: () => Promise<any> = async () => view(1);
  const seen: any[] = [];
  const w = new SpaceWatch(() => next(), (v) => seen.push(v), 1_000_000);
  w.poke(); await Bun.sleep(5);
  w.poke(); await Bun.sleep(5);
  expect(seen.length).toBe(1);
  next = async () => { throw new Error("pod read timed out"); };
  w.poke(); await Bun.sleep(5);
  expect(seen.length).toBe(2);
  expect(seen[1].workspaces).toEqual([{ id: "w1" }]);
  expect(seen[1].error).toContain("pod read timed out");
  next = async () => view(1);
  w.poke(); await Bun.sleep(5);
  expect(seen.length).toBe(3);
  expect(seen[2].error).toBeUndefined();
  w.stop();
});

test("never stacks polls", async () => {
  let calls = 0;
  let release!: () => void;
  const w = new SpaceWatch(() => (calls++, new Promise((r) => (release = () => r(view(1))))), () => {}, 1_000_000);
  w.poke(); w.poke(); w.poke();
  expect(calls).toBe(1);
  release(); await Bun.sleep(5);
  w.stop();
});
```

- [ ] **Step 2: Run to verify failure**

Run: `cd harness/packages/backend && bun test src/spacewatch.test.ts`
Expected: FAIL (module not found).

- [ ] **Step 3: Implement `spacewatch.ts`**

```ts
//! One space poller per bench (not one per TUI): every 5 s, after every turn and after a platform
//! tool. Emits only when the view changes. A failed read re-emits the last good view with `error`
//! set, so a TUI keeps its list and focus through a slow pod read instead of emptying.
import type { SpaceView } from "./index";

export class SpaceWatch {
  #timer?: ReturnType<typeof setInterval>;
  #inflight = false;
  #last?: SpaceView;
  #sent = "";
  constructor(private readonly read: () => Promise<SpaceView>, private readonly emit: (v: SpaceView) => void, private readonly everyMs = 5000) {}

  start(): void {
    if (this.#timer) return;
    this.poke();
    this.#timer = setInterval(() => this.poke(), this.everyMs);
  }
  stop(): void {
    clearInterval(this.#timer);
    this.#timer = undefined;
  }
  last(): SpaceView | undefined {
    return this.#last;
  }
  poke(): void {
    if (this.#inflight) return; // pod reads can outlast the beat; never stack them
    this.#inflight = true;
    this.read()
      .then((v) => {
        this.#last = v;
        this.#send(v);
      })
      .catch((e: any) => {
        const error = String(e?.message ?? e).slice(0, 200);
        this.#send(this.#last ? { ...this.#last, error } : { available: false, error, user: "", workspaces: [], environments: [] });
      })
      .finally(() => (this.#inflight = false));
  }
  #send(v: SpaceView): void {
    const s = JSON.stringify(v);
    if (s === this.#sent) return;
    this.#sent = s;
    this.emit(v);
  }
}
```

- [ ] **Step 4: Run unit tests**

Run: `cd harness/packages/backend && bun test src/spacewatch.test.ts`
Expected: 2 pass.

- [ ] **Step 5: Write the failing two-peer tests** (append to `sync.test.ts`)

```ts
test("settings.write and a finished login reach every TUI", async () => {
  await models();
  const local = new LocalBackend();
  const a = await pair(local), b = await pair(local);
  const eb: any[] = [];
  await b.watch((e) => eb.push(e));
  await a.settings.write({ thinkingLevel: "high" });
  await Bun.sleep(10);
  expect(eb.find((e) => e.type === "settings")?.settings.thinkingLevel).toBe("high");
});

test("forgetSessions tells the sessions watchers", async () => {
  const [m] = await models();
  const local = new LocalBackend();
  const h = await local.session("ws-9", { initial: { model: { provider: m.provider, id: m.id } }, fresh: true, tools: [] });
  const lists: any[][] = [];
  await local.sessions.watch((l) => lists.push(l));
  const n = lists.length;
  await (local as any).forgetWorkspace("ws-9");
  expect(lists.length).toBeGreaterThan(n);
  expect(lists.at(-1)!.some((s) => s.key.startsWith("ws-9"))).toBe(false);
  await h.dispose().catch(() => {});
}, 20000);
```

`forgetWorkspace(ws)` is the `LocalBackend` method the workspace-delete tool calls today to run `forgetSessions`; if it has another name, use that name here.

- [ ] **Step 6: Implement the pushes**

1. `settings.write(s)`: after persisting, `this.#broadcast({ type: "settings", settings: <the full merged settings object> })`.
2. `auth.login` success (and Task 8's Claude login): `this.#broadcast({ type: "auth_changed" })` after the Task 2 rebuilds.
3. `#space = new SpaceWatch(() => this.space(), (view) => this.#broadcast({ type: "space", view }))`. In `watch(cb)`: add cb, `this.#space.start()`, and if `this.#space.last()` exists call `cb({ type: "space", view: last })` at once; on off, when `#bench.size === 0` call `this.#space.stop()`.
4. In `baseHandle`'s subscription hook (via a new `hooks.onTool?(e)` and the existing `onEnd`): on `agent_end` → `this.#space.poke()`; on `tool_execution_end` whose `toolName` is a platform tool (the names `registryFor` registers from the workspaces/environments tool set, i.e. names starting `workspace_`, `environment_`, `service_`, `intercept_`) → `this.#space.poke()`.
5. `fs_changed`: on `tool_execution_end` of `write`, `edit`, `patch`, `bash`, `exec`, debounce 500 ms per target and broadcast `{ type: "fs_changed", ws }` where `ws` is the workspace id the session key belongs to (`key.split(":")[0]` when it is not `main`), else no `ws`:

```ts
  #fsTimers = new Map<string, ReturnType<typeof setTimeout>>();
  #fsChanged(ws?: string) {
    const t = ws ?? "";
    if (this.#fsTimers.has(t)) return;
    this.#fsTimers.set(t, setTimeout(() => { this.#fsTimers.delete(t); this.#broadcast(ws ? { type: "fs_changed", ws } : { type: "fs_changed" }); }, 500));
  }
```

   Use the same key-to-workspace helper `local.ts` already uses for `roleCard`.
6. `forget.ts`: `export async function forgetSessions(ws: string, live: …, changed: () => void)`; call `changed()` at the end. The caller in `local.ts` passes `() => this.#changed()`. Also `this.#cards.withdrawKey(k)` for every forgotten key (Task 4 step 6.5).

- [ ] **Step 7: Run to verify pass**

Run: `cd harness/packages/backend && bun test` then `cd harness && bun run check`
Expected: all pass.

- [ ] **Step 8: Commit**

```bash
git add -A harness/packages/backend/src
git commit -m "Push settings, logins, file changes and the space from the daemon"
```

---

### Task 6: TUI sync reducers (`sync.ts`)

**Files:**
- Create: `harness/apps/tui/src/sync.ts`
- Modify: `harness/apps/tui/src/sync.test.tsx`
- Modify: `harness/apps/tui/src/sessions.ts` only if `Entry`/`patchSession` types need the `id` on user rows

**Interfaces:**
- Consumes: `Entry`, `SessionView` (whatever `getSession` returns), `patchSession` from `sessions.ts`; `Ask`, `SessionState`, `SpaceView`, `BenchEvent` from `@kloudlite-tui/backend`.
- Produces (pure functions, no React):
  - `transcript(messages: any[], busy: boolean, toolSummary: (name: string, args: any) => string): { entries: Entry[]; history: string[] }` — ids `u${ts}` user, `m${ts}` agent, `m${ts}t` thinking, `toolCallId` tool; a tool with no result and `busy` true is `running`.
  - `userRow(event: any): Entry | null` — from `message_start` user with `shown`.
  - `upsertById(entries: Entry[], e: Entry): Entry[]`
  - `applyState(s: SessionState): Partial<SessionView>` → `{ model, tokens, queued }`
  - `keepFocus(prev: { focus: number; ids: string[] }, view: SpaceView): number` — focus index after a space push; unchanged when `view.error` is set.
  - `autoAnswer(ask: Ask, granted: Map<string, Set<string>>): string | null` — `"once"` when `ask.kind === "permission"` and `granted.get(ask.key)?.has(ask.tool)`, else `null`.
  - `grant(granted: Map<string, Set<string>>, ask: Ask): void`

- [ ] **Step 1: Write the failing tests** (append to `sync.test.tsx`)

```tsx
import { transcript, userRow, upsertById, applyState, keepFocus, autoAnswer, grant } from "./sync";

const sum = (n: string) => n;

test("snapshot then the same live event renders one row", () => {
  const msgs = [
    { role: "user", timestamp: 1, content: [{ type: "text", text: "hi" }] },
    { role: "assistant", timestamp: 2, content: [{ type: "text", text: "hello" }] },
  ];
  const { entries, history } = transcript(msgs, false, sum);
  expect(entries.map((e: any) => e.id)).toEqual(["u1", "m2"]);
  expect(history).toEqual(["hi"]);
  const live = userRow({ type: "message_start", message: { role: "user", timestamp: 1 }, shown: "hi" })!;
  expect(upsertById(entries, live).length).toBe(2);
});

test("a tool call with no result while busy is running", () => {
  const msgs = [{ role: "assistant", timestamp: 3, content: [{ type: "toolCall", id: "t1", name: "bash", arguments: {} }] }];
  expect((transcript(msgs, true, sum).entries[0] as any).status).toBe("running");
  expect((transcript(msgs, false, sum).entries[0] as any).status).toBe("ok");
});

test("a user message_start renders one row from shown", () => {
  expect(userRow({ type: "message_start", message: { role: "user", timestamp: 9 }, shown: "go" })).toEqual({ kind: "user", id: "u9", text: "go" });
  expect(userRow({ type: "message_start", message: { role: "assistant", timestamp: 9 } })).toBeNull();
});

test("pushed session_state sets model, tokens and queue", () => {
  const p = applyState({ type: "session_state", model: { provider: "openai", id: "x" }, thinkingLevel: "low", autoCompact: true, codemode: true, queued: { steering: ["a"], followUp: ["b"] }, tokens: 12 });
  expect(p).toEqual({ model: { provider: "openai", id: "x" }, tokens: 12, queued: [{ text: "a", kind: "steer" }, { text: "b", kind: "followUp" }] });
});

test("a space push with error keeps focus; a good push without the workspace clamps", () => {
  const v = (ids: string[], error?: string) => ({ available: true, user: "u", workspaces: ids.map((id) => ({ id })), environments: [], error }) as any;
  expect(keepFocus({ focus: 2, ids: ["a", "b"] }, v([], "timed out"))).toBe(2);
  expect(keepFocus({ focus: 2, ids: ["a", "b"] }, v(["b", "a"]))).toBe(1);
  expect(keepFocus({ focus: 2, ids: ["a", "b"] }, v(["a"]))).toBe(1);
  expect(keepFocus({ focus: 0, ids: ["a"] }, v([]))).toBe(0);
});

test("Allow always answers the next ask for that tool on that key only", () => {
  const g = new Map<string, Set<string>>();
  const ask = (key: string, tool: string) => ({ id: "x", key, kind: "permission", tool, title: "", options: [] }) as any;
  grant(g, ask("k", "bash"));
  expect(autoAnswer(ask("k", "bash"), g)).toBe("once");
  expect(autoAnswer(ask("other", "bash"), g)).toBeNull();
  expect(autoAnswer({ ...ask("k", "bash"), kind: "question" }, g)).toBeNull();
});
```

- [ ] **Step 2: Run to verify failure**

Run: `cd harness/apps/tui && bun test src/sync.test.tsx`
Expected: FAIL (`./sync` not found).

- [ ] **Step 3: Implement `sync.ts`**

Move `restoreTranscript`'s loop (`app.tsx:979-1033`) into `transcript()` with the ids added, and write the rest:

```ts
//! What a TUI does with what the daemon pushes. Pure: app.tsx owns React state and calls these.
//! The daemon holds every session's state; nothing here invents state the daemon did not send.
import type { Ask, SessionState, SpaceView } from "@kloudlite-tui/backend";
import type { Entry } from "./sessions";
import { fromSpace } from "./workspaces";

const text = (m: any) => (m.content ?? []).filter((b: any) => b.type === "text").map((b: any) => b.text).join("\n");

/** A transcript from an open's snapshot, with the ids the live path uses so later events update rows. */
export function transcript(messages: any[], busy: boolean, toolSummary: (name: string, args: any) => string) {
  const entries: Entry[] = [];
  const history: string[] = [];
  for (const m of messages) {
    if (m.role === "user") {
      const t = text(m);
      if (t) {
        entries.push({ kind: "user", id: `u${m.timestamp}`, text: t } as Entry);
        history.push(t);
      }
    } else if (m.role === "assistant") {
      const mid = `m${m.timestamp}`;
      const thinking = (m.content ?? []).filter((b: any) => b.type === "thinking").map((b: any) => b.thinking).join("");
      if (thinking.trim()) entries.push({ kind: "thinking", id: `${mid}t`, text: thinking, done: true });
      const t = (m.content ?? []).filter((b: any) => b.type === "text").map((b: any) => b.text).join("");
      if (t.trim()) entries.push({ kind: "agent", id: mid, text: t });
      for (const b of m.content ?? [])
        if (b.type === "toolCall")
          entries.push({ kind: "tool", id: b.id, name: b.name, summary: toolSummary(b.name, b.arguments), status: busy ? "running" : "ok" });
    } else if (m.role === "toolResult") {
      const i = entries.findIndex((e) => e.kind === "tool" && e.id === m.toolCallId);
      if (i === -1) continue;
      const t = text(m);
      entries[i] = { ...(entries[i] as Entry & { kind: "tool" }), status: m.isError ? "error" : "ok", output: t || undefined, error: m.isError ? t.split("\n")[0] : undefined, display: m.details?.display };
    }
  }
  return { entries, history };
}

export function userRow(e: any): Entry | null {
  if (e?.type !== "message_start" || e.message?.role !== "user") return null;
  return { kind: "user", id: `u${e.message.timestamp}`, text: e.shown ?? text(e.message) } as Entry;
}

export function upsertById(entries: Entry[], e: Entry): Entry[] {
  const id = (e as any).id;
  const i = entries.findIndex((x: any) => x.id === id);
  if (i === -1) return [...entries, e];
  const next = [...entries];
  next[i] = e;
  return next;
}

export function applyState(s: SessionState) {
  return {
    model: s.model,
    tokens: s.tokens,
    queued: [
      ...s.queued.steering.map((text) => ({ text, kind: "steer" as const })),
      ...s.queued.followUp.map((text) => ({ text, kind: "followUp" as const })),
    ],
  };
}

/** Focus after a space push: follows its workspace by id; an errored push never moves it. */
export function keepFocus(prev: { focus: number; ids: string[] }, view: SpaceView): number {
  if (view.error || prev.focus === 0) return prev.focus;
  const next = fromSpace(view).workspaces;
  const at = next.findIndex((w) => w.id === prev.ids[prev.focus - 1]);
  return at >= 0 ? at + 1 : Math.min(prev.focus, next.length);
}

/** Always-allow is this TUI's own (the person: "Let always allow be in Tui"). */
export function autoAnswer(ask: Ask, granted: Map<string, Set<string>>): string | null {
  return ask.kind === "permission" && granted.get(ask.key)?.has(ask.tool) ? "once" : null;
}

export function grant(granted: Map<string, Set<string>>, ask: Ask): void {
  const s = granted.get(ask.key) ?? new Set<string>();
  s.add(ask.tool);
  granted.set(ask.key, s);
}
```

If `Entry`'s `user` variant has no `id`, add `id?: string` to it in `sessions.ts`. If `fromSpace` filters workspaces (e.g. by owner) adjust the third `keepFocus` expectation to what `fromSpace` returns for those ids — do not change `fromSpace`.

- [ ] **Step 4: Run to verify pass**

Run: `cd harness/apps/tui && bun test`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add harness/apps/tui/src
git commit -m "Add the TUI's sync reducers for daemon pushes"
```

---

### Task 7: TUI consumes every push; local copies deleted

**Files:**
- Modify: `harness/apps/tui/src/app.tsx` (lines cited below are today's)
- Modify: `harness/apps/tui/src/app.test.tsx` (tests that relied on removed behaviour)
- Test: `harness/apps/tui/src/sync.test.tsx`

**Interfaces:**
- Consumes: Task 6 `transcript`, `userRow`, `upsertById`, `applyState`, `keepFocus`, `autoAnswer`, `grant`; Task 4 `backend().watch`, `backend().asks.answer`, `backend().mode.set`, `hello().asks`, `hello().mode`; Task 1 `handle.state`, `setCodemode`, `initial`; Task 2 `session_closed.reopen`.
- Produces: nothing for later tasks.

- [ ] **Step 1: Write the failing test** (append to `sync.test.tsx`, using the render harness the file's existing tests use and a fake backend whose `watch` captures its callback)

```tsx
test("an ask_resolved drops the card; an ask for a granted tool is answered without a card", async () => {
  const answered: [string, string][] = [];
  let push!: (e: any) => void;
  const fake = fakeBackend({
    watch: async (cb: any) => ((push = cb), () => {}),
    asks: { answer: async (id: string, c: string) => void answered.push([id, c]) },
  });
  const ui = await renderApp(fake);
  push({ type: "ask", ask: { id: "a1", key: "main", kind: "permission", tool: "bash", title: "Permission required", options: [{ id: "once", label: "Allow once" }, { id: "always", label: "Allow always" }, { id: "reject", label: "Reject" }] } });
  await ui.settle();
  expect(ui.frame()).toContain("Permission required");
  push({ type: "ask_resolved", id: "a1" });
  await ui.settle();
  expect(ui.frame()).not.toContain("Permission required");
});
```

`fakeBackend` and `renderApp` are the helpers `sync.test.tsx`/`app.test.tsx` already use to mount `App` against a fake backend; extend `fakeBackend` with defaults for `watch` (`async () => () => {}`), `asks`, `mode` and `hello().asks = []`, `hello().mode = "default"`. If the existing helpers have other names, use those names.

- [ ] **Step 2: Run to verify failure**

Run: `cd harness/apps/tui && bun test src/sync.test.tsx`
Expected: FAIL (card stays after `ask_resolved`).

- [ ] **Step 3: Rewire `app.tsx`**

1. **Bench stream** (new effect beside `sessions.watch` at 286-297): `backend().watch(onBench)` where `onBench(e: BenchEvent)`:
   - `ask`: `const auto = autoAnswer(e.ask, alwaysAllow.current); if (auto) void backend().asks.answer(e.ask.id, auto); else setAsks((l) => [...l, e.ask]);`
   - `ask_resolved`: `setAsks((l) => l.filter((a) => a.id !== e.id))`
   - `perm`: `setPermMode(e.mode)`
   - `settings`: update `prefs.thinkingLevel/autoCompact/codemode` and the default model only; never `vim`, `theme`, `sidebarWidth`, `sidebar`, `thinking`.
   - `auth_changed`: run what the login completion runs today (`loadProviderAuth().then(setAuth)` and the models refresh).
   - `fs_changed`: `if (focus > 0 && workspaces[focus - 1]?.id === e.ws) setFilesRefresh((n) => n + 1)` (read focus/workspaces through `live.current`).
   - `space`: `const f = keepFocus({ focus: live.current.focus, ids: live.current.workspaces.map((w) => w.id) }, e.view); setSpace(e.view); setFocus(f);`
   Seed `asks` from `hello().asks` and `permMode` from `hello().mode`.
2. **Space**: delete `refreshRef`, `inflight`, the 5 s interval (183-206) and `refreshRef.current()` in `agent_end` (643). Render `space.error` where the current error rendering reads it (unchanged component; the view now keeps workspaces).
3. **Asks**: `Ask` in the TUI becomes the backend's `Ask` plus the local-only help popup. Keep `pushAsk` only for the help popup (`openHelp`) and other local panels; delete `askQuestion`, the `tuiTools.push` block (852-874) and `gate` (909-976). The card's choice handler, for daemon asks: `if (choice === "always") grant(alwaysAllow.current, ask); void backend().asks.answer(ask.id, choice === "always" ? "once" : choice);` — the card disappears on `ask_resolved`, not on click. Sidebar mark for another key's ask: count `asks.filter((a) => a.key === k)` where the sidebar renders session rows.
4. **Mode**: shift+tab calls `backend().mode.set(next)` instead of `setPermMode`; `modeRef` deleted.
5. **Session state**: in `ensureAgent` (777-814) send only `initial: { model: getSession(sessions, key).model ?? defaultModel, thinkingLevel: prefs.thinkingLevel, autoCompact: prefs.autoCompact === "on", codemode: prefs.codemode === "on" }`, `fresh`, `tools: tuiTools`; after open `setSessions((map) => patchSession(map, key, { ...applyState(agent.state), busy: agent.busy }))`. In `handleAgentEvent` add `case "session_state": setSessions((map) => patchSession(map, key, applyState(event)))`. Delete the token sum at 670-672 and the `queue_update` case (733-742) — state carries both.
6. **Reopen**: `session_closed` with `event.reopen` → after the existing listener drops the dead entry, `ensureAgent(key).catch(() => {})` if `key` is the active key or any view holds it (`getSession(sessions, key).entries.length > 0`). Delete `reopen()` (817-825).
7. **Transcript**: `restoreTranscript` becomes `const { entries, history } = transcript(agent.messages, agent.busy, toolSummary); setSessions((map) => patchSession(map, key, (s) => ({ entries: foldRetries(entries), history: s.history.length ? s.history : history, restored: true })));` — always replaces entries. Open with `fresh` still calls it (empty messages give empty entries).
8. **User rows**: `handleAgentEvent` `message_start`: `const row = userRow(event); if (row) setSessions((map) => patchSession(map, key, (s) => ({ entries: upsertById(s.entries, row) })));`. In `submit` delete the `append(key, { kind: "user", … })` (1264); keep the image count by sending it with the prompt as today (the row from the event shows the text; drop the `images` badge if the event carries no count — note it in the commit body).
9. **Names**: delete `sessionNames`/`sessionDescs` state (230-237) and the auto-title block (1252-1257). Read `name`/`description` from `watched` (`watched.find((m) => m.key === k)`). `/session name|desc` call the backend only.
10. **Clear**: `/clear` (1091-1106) becomes `void backend().sessions.clear(activeKey).catch((e) => append(activeKey, { kind: "error", text: String(e.message ?? e) }))` — the `session_closed { reopen: true }` path empties and reopens every view, this one included. Delete `clearedSeen` (298-308).
11. **Model / codemode / settings**: `/model` (1149-1178) becomes `setModel` on the handle (the daemon decides rebuild) plus `settings.write({ defaultModel })`; delete the `isClaude` rebuild branch. `/settings codemode` (1217-1230): `settings.write({ codemode })` only (the daemon rebuilds idle agents and refuses nothing; busy ones rebuild at `agent_end`); delete the `running` check. `/settings thinkingLevel` and `autoCompact` keep calling each live handle (the daemon pushes `session_state` back).
12. Login reopen: delete whatever the login completion does to reopen Claude sessions (the daemon does it, Task 2).
13. `app.test.tsx`: delete or rewrite tests that asserted removed behaviour (local `cleared` handling, local user-row append, TUI-side gate decisions, `reopen`). Keep every other test passing.

- [ ] **Step 4: Run to verify pass**

Run: `cd harness/apps/tui && bun test` then `cd harness && bun run check` then `wc -l harness/apps/tui/src/app.tsx`
Expected: all pass; `app.tsx` below 1648 lines.

- [ ] **Step 5: Commit**

```bash
git add -A harness/apps/tui/src
git commit -m "Render every session, card and space push in the TUI"
```

---

### Task 8: Claude login over the wire

**Files:**
- Create: `harness/packages/backend/src/claudelogin.ts`
- Modify: `harness/packages/backend/src/local.ts` (`auth.login` 498: provider `claude-subscription`)
- Modify: `harness/packages/agent/src/claude.ts:53` (`AUTH_MESSAGE`)
- Modify: `harness/apps/tui/src/app.tsx` (`/login` options list)
- Test: `harness/packages/backend/src/claudelogin.test.ts`

**Interfaces:**
- Consumes: the `auth.login` event shape (`{ type: "url", url } | { type: "prompt", message } | { type: "done" } | { type: "error", message }` — use the exact shape `auth.login` emits for pi OAuth today) and the `auth.prompt` request remote.ts already handles.
- Produces: `claudeLogin(onEvent: (e: AuthEvent) => void, ask: (message: string) => Promise<string>, spawnFn = Bun.spawn): Promise<void>`.

- [ ] **Step 1: Probe the real prompts (bench, read-only)**

On the user's bench, in a scratch tmux pane (never the user's processes): `script -qfec "claude auth login" /dev/null` and record exactly (a) the line that carries the sign-in URL, (b) the prompt text that asks for the code, (c) the success line. Write them into the test fixture below verbatim. If `claude auth login` offers a method menu first, record the keystroke that picks the subscription method.

- [ ] **Step 2: Write the failing test**

```ts
import { test, expect } from "bun:test";
import { claudeLogin } from "./claudelogin";

// Lines below are the probe's verbatim output (Step 1); replace each string with what was recorded.
const URL_LINE = "Browser didn't open? Use the url below to sign in:\nhttps://claude.ai/oauth/authorize?code=true&client_id=x";
const CODE_PROMPT = "Paste code here if prompted >";
const DONE_LINE = "Login successful.";

test("relays the URL, answers the code prompt, reports done", async () => {
  const written: string[] = [];
  const events: any[] = [];
  const fakeSpawn: any = () => {
    const out = new ReadableStream({ start(c) { const e = new TextEncoder(); c.enqueue(e.encode(`${URL_LINE}\n${CODE_PROMPT}`)); setTimeout(() => { c.enqueue(e.encode(`\n${DONE_LINE}\n`)); c.close(); }, 20); } });
    return { stdout: out, stdin: { write: (s: string) => void written.push(s), end() {} }, exited: Promise.resolve(0) };
  };
  await claudeLogin((e) => events.push(e), async () => "CODE123", fakeSpawn);
  expect(events.find((e) => e.type === "url")?.url).toStartWith("https://claude.ai/oauth/authorize");
  expect(written.join("")).toBe("CODE123\n");
  expect(events.at(-1)?.type).toBe("done");
});
```

- [ ] **Step 3: Run to verify failure**

Run: `cd harness/packages/backend && bun test src/claudelogin.test.ts`
Expected: FAIL (module not found).

- [ ] **Step 4: Implement**

```ts
//! `claude auth login` run by the daemon under a pty (`script -qfec`), so the bench needs no ssh:
//! the sign-in URL goes to the TUI as an auth `url` event, the pasted code comes back as the
//! `auth.prompt` answer — the same path pi's OAuth logins take.
export async function claudeLogin(onEvent: (e: any) => void, ask: (message: string) => Promise<string>, spawnFn: typeof Bun.spawn = Bun.spawn): Promise<void> {
  const p: any = spawnFn(["script", "-qfec", "claude auth login", "/dev/null"], { stdin: "pipe", stdout: "pipe", stderr: "pipe" });
  const dec = new TextDecoder();
  let buf = "", sentUrl = false, asked = false;
  for await (const chunk of p.stdout as any) {
    buf += dec.decode(chunk, { stream: true });
    const url = buf.match(/https:\/\/\S+/)?.[0];
    if (url && !sentUrl) { sentUrl = true; onEvent({ type: "url", url }); }
    if (!asked && /paste code/i.test(buf)) {
      asked = true;
      const code = await ask("Paste the code from the browser");
      p.stdin.write(`${code.trim()}\n`);
    }
  }
  const code = await p.exited;
  if (code !== 0 || !/success/i.test(buf)) throw new Error(`claude login failed: ${buf.trim().split("\n").at(-1) ?? code}`);
  onEvent({ type: "done" });
}
```

Adjust the two regexes to the probe's recorded strings (URL line and code prompt) and the success test to the recorded success line. In `local.ts` `auth.login(provider, …)`: `if (provider === "claude-subscription") return claudeLogin(onEvent, (m) => prompt(m))` using the same `onEvent`/`prompt` plumbing pi OAuth uses there, then the Task 2 rebuild and the Task 5 `auth_changed`. In the TUI `/login` provider list add `{ provider: "claude-subscription", label: "Claude (subscription)" }` beside the existing entries. `AUTH_MESSAGE` (`claude.ts:53`): "Claude is not signed in on this bench — run /login in the TUI and pick Claude (subscription)."

- [ ] **Step 5: Run to verify pass**

Run: `cd harness/packages/backend && bun test` then `cd harness && bun run check`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add -A harness/packages harness/apps/tui/src
git commit -m "Sign in to Claude from the TUI through the daemon"
```

---

### Task 9: No ssh on the bench

**Files:**
- Modify: `bins/kl-connect/src/bench.rs` (delete `Mode`/`modes`/`KL_DIRECT` ~65, fallback loop 70-96, `ssh_argv` 144-190, clip 56-63, `claude_login` 195-218; tests from 339)
- Modify: `bins/kl-connect/src/main.rs` (drop `--remote-tui`, the `claude login` subcommand, line-1 doc); delete `bins/kl-connect/src/clip.rs` if nothing else uses it; keep `sshconfig::safe_name`
- Modify: `bins/gateway/src/tunnel.rs` (remove `bench_port` line 65 and its test ~390), `bins/gateway/src/main.rs:62` (`BENCH_PORT`) and comment `:85`, `bins/gateway/src/resolve.rs:14` comment
- Modify: `crates/workspaces/src/k8s/bench.rs` (remove `BENCH_PORT`, ssh port ~113, netpol port ~191, docs 11-13/46, authorized_keys mount 124-126), `crates/workspaces/src/k8s/tests/bench.rs` (lines 68, 177; delete `a_bench_mounts_the_owners_authorized_keys` ~80)
- Modify: `deploy/bench/Dockerfile` (line 15 drop `openssh-server`, line 57 drop the `sshd_config` COPY, comments 1, 12-13, 30, 64-65)
- Delete: `bench/sshd_config`, `bench/sv/sshd/`, `bench/term/login-shell` (first `grep -rn -e login-shell bench/ deploy/` — if `bench/sv/term/run` uses it, point it at `/bin/bash -l` and say so in the commit body)
- Modify: `bench/sv/kl-host/run` comment only (it runs the daemon; it stays — correct spec §12 which lists it for deletion), `bench/sessions/main.ts` comments 5-6 and 41
- Modify: `harness/apps/tui/src/remote-args.ts` (drop `--ssh`), `harness/apps/tui/src/remote.tsx` (comments, usage), `harness/packages/backend/src/remote.ts` header, `harness/packages/backend/src/daemon.ts` header 1-10
- Modify: `docs/superpowers/specs/2026-10-09-tui-state-sync-design.md` §12: `bench/sv/kl-host/` is the daemon's runner and stays

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `kl-connect [team]` always runs `kl-tui --pipe <kl-connect> bench-proxy --tui [team]`; a bench ticket on gateway `/tunnel/{ws}` is 401.

- [ ] **Step 1: Write the failing tests**

In `bins/gateway/src/tunnel.rs` tests (replace the `bench_port` test ~390):

```rust
#[tokio::test]
async fn a_bench_ticket_on_the_tunnel_is_refused() {
    // the bench has no sshd: its only doors are /tui/ (kl-tui) and ttyd
    let res = tunnel_with_ticket(bench_ticket()).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}
```

Use the test helpers this file's existing tunnel tests use to build a request with a ticket and a bench target (the removed `bench_port` test built one); name them as they are named there.

In `bins/kl-connect/src/bench.rs` tests:

```rust
#[test]
fn the_bench_runs_kl_tui_over_the_pipe() {
    let argv = tui_argv(Path::new("/opt/kl/kl-connect"), Some("team-a"));
    assert_eq!(argv, ["--pipe", "/opt/kl/kl-connect", "bench-proxy", "--tui", "team-a"]);
}

#[test]
fn a_missing_kl_tui_names_the_path() {
    let err = kl_tui_beside(Path::new("/nowhere/kl-connect")).unwrap_err().to_string();
    assert!(err.contains("/nowhere/kl-tui"), "{err}");
}
```

`tui_argv` and `kl_tui_beside` are the argv builder and the sibling-binary lookup `bench.rs` already has for the direct mode; if they have other names, rename the test calls, not the functions.

In `crates/workspaces/src/k8s/tests/bench.rs`:

```rust
#[test]
fn a_bench_exposes_no_ssh_port() {
    let pod = bench_pod(&sample_bench());
    let ports: Vec<i32> = pod.containers().flat_map(|c| c.ports.iter().flatten().map(|p| p.container_port)).collect();
    assert!(!ports.contains(&22) && !ports.contains(&2222), "{ports:?}");
}
```

Use the builder and fixture names the file's existing tests use (lines 68, 177).

- [ ] **Step 2: Run to verify failure**

Run: `cd /Volumes/kdisk/rustic-git-wt/tui-clear-sync && CARGO_TARGET_DIR=/Volumes/kdisk/target cargo test -p gateway -p kl-connect -p kloudlite-workspaces 2>&1 | tail -30`
Expected: the three new tests FAIL (tunnel accepts a bench ticket; ssh port present). Use each crate's real package name from its `Cargo.toml`.

- [ ] **Step 3: Implement** — the deletions listed under Files. `kl-connect [team]`:

```rust
pub fn run(team: Option<&str>) -> anyhow::Result<std::process::ExitStatus> {
    let me = std::env::current_exe()?;
    let tui = kl_tui_beside(&me)?;
    // exit 3 = protocol mismatch between this kl-tui and the bench: report it, never retry another way
    let status = std::process::Command::new(&tui).args(tui_argv(&me, team)).status()?;
    if status.code() == Some(3) {
        anyhow::bail!("kl-tui and the bench speak different protocols — update kl-connect/kl-tui");
    }
    Ok(status)
}
```

`bench-proxy`: delete the plain (sshd) mode; `--tui` stays accepted and is the only behaviour. The proxy always takes the `/tui/` URL (`replacen("/tunnel/", "/tui/", 1)` — check `crates/workspaces/src/api/workspaces/mod.rs:619` for the URL it is handed and keep that replacement).

- [ ] **Step 4: Run to verify pass**

Run: `CARGO_TARGET_DIR=/Volumes/kdisk/target cargo test -p gateway -p kl-connect -p kloudlite-workspaces 2>&1 | tail -5` then `CARGO_TARGET_DIR=/Volumes/kdisk/target cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -5` then `cd harness && bun run check` then `grep -rn -e sshd -e openssh -e BENCH_PORT -e KL_DIRECT bins crates deploy/bench bench harness --include='*' | grep -v -e 'ws ssh' -e workspace | head`
Expected: tests and clippy pass; grep shows no bench ssh leftovers (workspace ssh lines are expected and stay).

- [ ] **Step 5: Commit**

```bash
git add -A bins crates deploy/bench bench harness docs/superpowers/specs/2026-10-09-tui-state-sync-design.md
git commit -m "Remove ssh from the bench"
```

---

## Drill (after shipping, laptop, two tmux TUIs on the user's bench)

For each of `/clear`, `/model` across Claude/pi, codemode, a prompt, a permission card answered in the other TUI, abort with a card open, workspace create and delete, `/login`: the other pane shows the result within one second (space within 5 s), checked by `tmux capture-pane -p`. Never touch the user's own kl-connect / kl-tui / bench-proxy processes.

## Spec coverage

| Spec | Task |
| --- | --- |
| §1 protocol 2 | 1 |
| §2 session state | 1 |
| §3 opens carry no state, `setCodemode` | 1 (state), 2 (rebuild) |
| §4 rebuilds, `reopen`, serialised opens, `cleared` removed | 2 |
| §5 transcript replace with live ids, running tools | 6, 7 |
| §6 user messages from events, `shown` | 3, 6, 7 |
| §7 names from the list, auto-title in daemon | 3, 7 |
| §8 cards, mode, always-allow per TUI, hello asks, `clients.ts` deleted | 4, 6, 7 |
| §9 settings and auth pushed | 5, 7 |
| §10 space pushed, error keeps list, forget notifies | 5, 6, 7 |
| §11 files nudged | 5, 7 |
| §12 no ssh on the bench, Claude login on the wire | 8, 9 |
