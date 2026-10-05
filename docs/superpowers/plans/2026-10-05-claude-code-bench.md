# Claude Code Bench Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The bench pod runs Claude Code (main session in tmux, one Agent SDK session per workspace)
with a `kloudlite` mod that registers each workspace's `kl ide serve` tools as Claude Code tools.
The sys-1 engine is deleted.

**Architecture:** One container, `sessions`, under runit as uid 1000, with four services: tmux
running `claude`, ttyd, sshd and `kl-sessions`. The mod lives in `bench/mod/`. It reads `/v1` with
the bench-tool token and calls `POST {addr}/tools/{name}` with the workspace token that
`/v1/workspaces/{id}/tools` returns. The provider is `ANTHROPIC_BASE_URL` (ClusterSettings) plus a
per-user `provider-token` (keys beat). The gateway gains an HTTP/WS proxy so the browser can reach
ttyd, and the existing byte tunnel carries `kl-connect bench`'s SSH.

**Tech Stack:** Rust (axum, kube, jsonwebtoken), Node 24 (`.ts` type stripping, `node:test`),
`@anthropic-ai/claude-agent-sdk`, the Claude Code CLI and mods, runit, tmux, ttyd, OpenSSH.

**Spec:** `docs/superpowers/specs/2026-10-05-claude-code-bench-design.md`

## Rulings made while planning (the survey contradicted the spec; owner reviews these with the plan)

- R1 — **`bench_session` is KEPT.** The spec listed it for deletion, but it mints the 60 s gateway
  tunnel token that `kl-connect bench` and the web terminal both need. What goes is the
  desktop-only `POST/DELETE /v1/bench/tool-token` (`mint_tool_token` requires `caller.parent`, and no
  desktop is involved any more).
- R2 — **The bench-tool token is minted by the keys beat**, which writes the existing `bench-tool`
  Secret for every bench whose member is not Paused. TTL is `WORKSPACE_TOOL_TTL_SECS` (86400) and
  `parent` is the bench id. Without this the mod has no `/v1` credential: `workspace-token`'s
  audience (`WORKSPACE_TOOL_ROUTES`) lacks `GET /v1/workspaces` and `/tools`. Revocation means the
  beat stops writing it, the same model as `workspace-token`. Probe `bench.tool.revoked` is retired
  and `bench.tool.token`/`audience` mint with `kloudlite-jwt` directly.
- R3 — **The bench has no runit today** (single `harness-bench` process). Task R covered the
  workspace pod only. The bench image adds runit, tmux, ttyd and openssh-server from Debian apt,
  runs `runsvdir` as uid 1000 (sshd non-root on 7789, host keys under `~/.ssh-host`), and mounts
  the owner's `authorized_keys` the way the workspace pod does.
- R4 — **Ports.** `BENCH_PORT` (7789) becomes sshd: the gateway's byte tunnel already targets it,
  so `kl-connect bench` changes only on the client side. ttyd moves to the new `BENCH_TERM_PORT`
  7681, reached only through the gateway's new `/term/{bench}/` proxy.
- R5 — **Node 24**, not 22. The existing bench image and harness gate are 24, and `.ts` type
  stripping is unflagged there.
- R6 — **Layout.** New code goes in top-level `bench/` (`bench/mod/`, `bench/sessions/`,
  `bench/sv/`). The desktop app under `harness/src` is out of scope and untouched. `harness/bench`,
  `harness/pi` and `harness/skills` are deleted if `harness/src` does not import them; otherwise
  only `harness/bench`.
- R7 — **`spec.bench.model` stays**, stamped as `ANTHROPIC_MODEL` in place of `KL_MODEL`.

## Global Constraints

- Cargo runs on the laptop only, with `CARGO_TARGET_DIR=/Volumes/kdisk/target-sys1-wire`. Never in
  the dev pod.
- Worktree `/Volumes/kdisk/rustic-git-wt/sys1-wire`, branch `claude-bench`. Prefix every command
  with `cd /Volumes/kdisk/rustic-git-wt/sys1-wire &&`. Git steps one at a time. Never stash.
- Commit subjects are imperative sentence case with no tool attribution. The commit-msg hook
  rejects the word "Claude", so call it "the agent CLI" in messages.
- Gate per Rust task: `cargo clippy --workspace --all-targets -- -D warnings` and the touched
  crates' tests. Gate per Node task: `node --test` in that package.
- No compat code, no migrations, no backfills (dev phase). Never print a secret value.
- Comments explain WHY in the style of `bins/server/src/router/route.rs`. Deliberate shortcuts
  carry `// ponytail: <ceiling and upgrade path>`. Files stay under ~800 lines with a `//!` header.
- Do not push and do not ship. Shipping (Dagger, pin, roll, probe) is the controller's step after
  the final review.
- Disabled built-ins, verbatim: `Read Write Edit MultiEdit Bash Grep Glob NotebookEdit WebFetch`.
- `kl-sessions` listens on `127.0.0.1:8917` only.

## Review Focus

1. **Workspace stopped mid-turn.** The tool call returns `workspace … is stopped; start it` as a
   tool error and never hangs. Test in Task 3.
2. **Workspace token rotated** (tool server 401). The mod re-fetches `/v1/workspaces/{id}/tools`
   once and retries once. Test in Task 3.
3. **Message sent while busy.** It queues, shows in `/state.queued`, and runs after the current
   turn. Test in Task 2.
4. **Resume of a missing or corrupt session id** after a wake. The service starts fresh and
   records `s:(error) resume failed: …`, without crash-looping. Test in Task 2.
5. **Mod fails to load or `/v1` is down at `session.start`.** The session has no tools and the
   built-ins stay denied. Test in Task 3.

---

### Task 0: Spike — the mods API shape and how registered tools render (throwaway)

**Files:** scratch only, under
`/private/tmp/claude-501/-Users-karthik-rustic-git/ca5119e6-5c89-409b-a058-b668d4457515/scratchpad/spike-mod/`.
Nothing is committed.

- [ ] **Step 1:** Read `https://code.claude.com/docs/en/plugins/mods/reference` and `/events`
  (WebFetch). Record the exact signatures of `$.tool.register` (name, description, JSON schema
  input, handler return shape for success and for error), `tool.check`'s return,
  `prompt.context`'s return, and the Agent SDK `query()` options `plugins`, `disallowedTools` and
  `env` (`https://code.claude.com/docs/en/agent-sdk/typescript`).
- [ ] **Step 2:** Build a 30-line mod in the scratch dir. It registers `echo_edit` with input
  `{path, old_string, new_string}` and returns a unified diff string. Run
  `claude --plugin-dir <dir>` locally and ask it to call `echo_edit`. Screenshot or describe how
  the call and result render.
- [ ] **Step 3:** Write the findings to
  `.superpowers/sdd/2026-10-05-claude-code-bench/spike.md`: exact API signatures, and a verdict of
  either "renders readable" (Task 3 adds no renderer) or "unreadable" (Task 3 adds the `ToolUse`
  and `ToolResult` renderers for `edit`/`patch`/`exec` described there). Delete the scratch dir.

### Task 1: Delete the sys-1 engine and Kompress

**Files:**
- Delete: `harness/bench/` and, per R6, `harness/pi/` and `harness/skills/`. Check first with
  `grep -rn "pi/\|skills/\|bench/" harness/src harness/package.json harness/tsconfig*.json harness/vite.config.ts`.
- Modify: `crates/workspaces/src/crd/settings.rs` to remove `kompress_url` (default fn line 57,
  field line 177, META row line 222).
- Modify: `crates/workspaces/src/k8s/policies.rs` to remove `allow_bench_kompress` (line 302) and
  its call site.
- Modify: `crates/workspaces/src/k8s/bench.rs` to remove the `kompress_url` param,
  `bench_engine_var` and the six engine env vars (lines 30-39, 99-106, 114-116).
- Modify: `crates/workspaces/src/k8s/secrets.rs` to remove `BENCH_ENGINE_SECRET` (line 18).
- Modify: `crates/workspaces/src/k8s/workspace.rs:637-642` and
  `bins/agent/src/controller/workspace/mod.rs:426` to drop `kompress_url` from the bench tuple.
- Modify: everything `grep -rn "kompress\|Kompress\|bench-engine\|BENCH_ENGINE" --include='*.rs' --include='*.yaml' --include='*.md' crates bins deploy`
  finds, including the Kompress deploy yaml and `deploy/dev/pod/harness-gate.sh`'s bench step.
- Modify: `deploy/dev/pod/ship.sh:149-154` to stop copying `harness/bench harness/pi harness/skills`.
  Task 6 re-adds `bench/`.
- Test: the existing `crates/workspaces/src/k8s/tests/bench.rs` and `api_admin_settings`/schema
  tests, adjusted.

- [ ] **Step 1:** Run the greps above and list every hit in the report file before editing.
- [ ] **Step 2:** Delete the directories with `git rm -r` and make the Rust edits. Where a test
  asserted a Kompress env var or `allow-bench-kompress` policy, delete that assertion. Where a test
  asserted the engine Secret vars, assert their absence instead:

```rust
assert!(c.env.as_ref().unwrap().iter().all(|e| !e.name.ends_with("_API_KEY")),
    "no provider key is stamped into a bench; the token comes from user-key");
```

- [ ] **Step 3:** `CARGO_TARGET_DIR=/Volumes/kdisk/target-sys1-wire cargo clippy --workspace --all-targets -- -D warnings`
  then `cargo test -p kloudlite-workspaces` and `cargo test -p kloudlite-agent`. Expected: PASS.
- [ ] **Step 4:** Commit: `Delete the sys-1 engine and the Kompress setting`.

### Task 2: `kl-sessions`: one warm SDK session per workspace

**Files:**
- Create: `bench/sessions/package.json` (`"type":"module"`, dep `@anthropic-ai/claude-agent-sdk`
  pinned to the exact version `npm view @anthropic-ai/claude-agent-sdk version` prints, plus a
  `package-lock.json` from `npm install`).
- Create: `bench/sessions/sessions.ts` (pure state machine, no I/O beyond the injected `query`).
- Create: `bench/sessions/main.ts` (HTTP server plus `--ping`).
- Test: `bench/sessions/sessions.test.ts`.

**Interfaces:**
- Produces HTTP on `127.0.0.1:8917`:
  - `POST /send {ws, text, agentId?}` returns `{}`.
  - `GET /state` returns `{[ws]: {lines: string[], busy: boolean, queued: string[], agents: {id,label,status,lines}[]}}`.
  - `GET /idle` returns `{clients: number, busy: boolean, idleSince: number|null}`.
- Produces CLI `node /opt/kl/sessions/main.ts --ping`, which exits 0 while not idle for
  `KL_BENCH_IDLE_SECS` and 1 once idle. This keeps the contract `bins/agent/.../bench.rs:122`
  reads.
- Line prefixes are the demo's: `u:` user, `a:` assistant, `t:` tool call, `r:` result, `s:` system.

- [ ] **Step 1: Write the failing tests.**

```ts
// bench/sessions/sessions.test.ts
import { test } from "node:test";
import assert from "node:assert/strict";
import { Sessions } from "./sessions.ts";

// A fake query(): yields an init, echoes each user message as one assistant text, then a result.
function fakeQuery(log: unknown[]) {
  return ({ prompt, options }: any) => (async function* () {
    log.push(options);
    yield { type: "system", subtype: "init", session_id: "sid-1" };
    for await (const m of prompt) {
      yield { type: "assistant", parent_tool_use_id: null, message: { content: [{ type: "text", text: `echo ${m.message.content}` }] } };
      yield { type: "result" };
    }
  })();
}

test("a session gets its workspace, the mod, and the built-ins disabled", async () => {
  const log: any[] = [];
  const s = new Sessions({ query: fakeQuery(log), home: "/h", modDir: "/opt/kl/mod", saved: {} });
  s.send("ws-a", "hi");
  await s.settled("ws-a");
  const o = log[0];
  assert.equal(o.env.KL_WORKSPACE, "ws-a");
  assert.equal(o.cwd, "/h/sessions/ws-a");
  assert.deepEqual(o.plugins, [{ type: "local", path: "/opt/kl/mod" }]);
  assert.deepEqual(o.disallowedTools, ["Read","Write","Edit","MultiEdit","Bash","Grep","Glob","NotebookEdit","WebFetch"]);
});

test("a second message while busy queues and runs after", async () => {
  const s = new Sessions({ query: fakeQuery([]), home: "/h", modDir: "/m", saved: {} });
  s.send("ws-a", "one");
  s.send("ws-a", "two");
  assert.deepEqual(s.state()["ws-a"].queued, ["two"]);
  await s.settled("ws-a");
  assert.deepEqual(s.state()["ws-a"].lines.filter((l: string) => l.startsWith("a:")), ["a:echo one", "a:echo two"]);
  assert.equal(s.state()["ws-a"].busy, false);
});

test("subagent messages land under agents, not the parent's lines", async () => {
  const q = () => (async function* (this: void) {
    yield { type: "system", subtype: "init", session_id: "s" };
    yield { type: "assistant", parent_tool_use_id: "tu-9", message: { content: [{ type: "tool_use", name: "read", input: { path: "a.rs" } }] } };
    yield { type: "result" };
  })();
  const s = new Sessions({ query: q as any, home: "/h", modDir: "/m", saved: {} });
  s.send("ws-a", "go");
  await s.settled("ws-a");
  const st = s.state()["ws-a"];
  assert.equal(st.agents[0].id, "tu-9");
  assert.deepEqual(st.agents[0].lines, ["t:read(a.rs)"]);
  assert.ok(!st.lines.includes("t:read(a.rs)"));
});

test("a failed resume starts fresh and says why, once", async () => {
  let calls = 0;
  const q = ({ options }: any) => (async function* () {
    calls++;
    if (options.resume) throw new Error("No conversation found with session ID: dead");
    yield { type: "system", subtype: "init", session_id: "new" };
    yield { type: "result" };
  })();
  const s = new Sessions({ query: q as any, home: "/h", modDir: "/m", saved: { "ws-a": { lines: [], sessionId: "dead" } } });
  s.send("ws-a", "hi");
  await s.settled("ws-a");
  assert.equal(calls, 2);
  assert.ok(s.state()["ws-a"].lines.some((l: string) => l.startsWith("s:(error) resume failed")));
});

test("idle: no clients and nothing busy", () => {
  const s = new Sessions({ query: fakeQuery([]), home: "/h", modDir: "/m", saved: {} });
  assert.equal(s.busy(), false);
});
```

- [ ] **Step 2:** `cd bench/sessions && node --test` fails (no `sessions.ts`).
- [ ] **Step 3: Implement `sessions.ts`.** Port the demo `service/server.mjs` (lines 24-117) into
  `class Sessions { constructor({query, home, modDir, saved}); send(ws, text, agentId?); state(); busy(); settled(ws): Promise<void>; toJSON() }`.
  The changes from the demo:
  - Options are `{cwd: \`${home}/sessions/${ws}\`, env: {...process.env, KL_WORKSPACE: ws}, plugins: [{type:'local', path: modDir}], disallowedTools: DISABLED, permissionMode: 'bypassPermissions', includePartialMessages: true, ...(sessionId ? {resume: sessionId} : {})}`.
    `DISABLED` is the exported constant from Global Constraints. Use the option names Task 0's
    `spike.md` recorded if they differ. Drop the demo's `model: 'sonnet'` (R7: `ANTHROPIC_MODEL`
    env decides).
  - Messages with non-null `parent_tool_use_id` append to `agents[id].lines`. The label is the
    `description` of the parent `Agent` tool_use input when seen, else the id. Status becomes
    `done` on the next `result`.
  - A throw while `resume` is set pushes `s:(error) resume failed: <first 200 chars>`, clears
    `sessionId` and restarts once without resume. Any other throw pushes `s:(error) …`, marks the
    session not busy, and deletes it so the next `send` restarts it (demo behaviour).
  - `settled(ws)` resolves when the session is not busy and the queue is empty. It is for tests
    and needs no timer.
  - `mkdirSync(cwd, {recursive: true})` before `query`.
- [ ] **Step 4: Implement `main.ts`.**
  - Persist to `${HOME}/sessions/sessions.json` (whole write on each `result`; keep the demo's
    ponytail comment).
  - Serve `/send`, `/state` and `/idle` (`clients` = line count of
    `tmux list-clients -t kl 2>/dev/null` via `execFileSync`, 0 on error). Track `idleSince` as the
    first moment both `clients == 0` and `!busy`, reset on either change, sampled every 5 s.
  - `--ping`: GET `/idle`. Exit 1 iff `idleSince` is set and
    `now - idleSince >= KL_BENCH_IDLE_SECS*1000`. Exit 0 if the server answers otherwise. Exit 2 if
    it is unreachable (the startup probe covers boot).
  - `import { query } from "@anthropic-ai/claude-agent-sdk"`.
- [ ] **Step 5:** `node --test` passes all five tests.
- [ ] **Step 6:** Commit: `Run one warm agent session per workspace on the bench`.

### Task 3: The kloudlite mod: workspace tools as tools

**Files:**
- Create: `bench/mod/.claude-plugin/plugin.json` (`{"name":"kloudlite","version":"0.1.0"}`).
- Create: `bench/mod/hooks/hooks.json` (`{"modules":["./register.tsx"]}`).
- Create: `bench/mod/lib/platform.ts` (pure, testable: `/v1` and tool-server clients over an
  injected `fetch`).
- Create: `bench/mod/hooks/register.tsx` (thin: wires events to `lib/`).
- Test: `bench/mod/lib/platform.test.ts`.

**Interfaces:**
- Consumes: `KL_WORKSPACE`, `KL_API_URL`, `KL_TOOL_TOKEN_FILE` (`/etc/kloudlite/bench-tool/token`,
  read per call; the beat rotates it, R2).
- Produces in `lib/platform.ts`:
  - `toolsAt(fetch, api, token, ws): Promise<{address: string, token?: string}>`
    (`GET {api}/v1/workspaces/{ws}/tools`; a non-2xx throws `Error(body.error)`).
  - `listTools(fetch, at): Promise<{name: string, description: string, input_schema: object}[]>`
    (`GET http://{address}/tools`).
  - `callTool(fetch, getAt, ws, name, input): Promise<{ok: true, text: string} | {ok: false, error: string}>`.
    On 401 it calls `getAt(true)` (refetch) and retries once. On a connection error it returns
    `{ok:false, error: \`workspace ${ws} is unreachable: ${e.message}; start it\`}`.
  - `DISABLED` (same list as Global Constraints).
  - `workspaceContext(fetch, getAt): Promise<string>` (the `read` of `~/workspace/CLAUDE.md` plus
    `GET /fs/git`, each section skipped on error).

- [ ] **Step 1: Write the failing tests** (`node --test`, fake `fetch`):

```ts
// bench/mod/lib/platform.test.ts
import { test } from "node:test";
import assert from "node:assert/strict";
import { callTool, toolsAt } from "./platform.ts";

const res = (status: number, body: unknown) => new Response(JSON.stringify(body), { status });

test("a stopped workspace is a tool error naming it, not a hang", async () => {
  const f = async () => res(409, { error: "workspace api is Stopped; start it to run tools" });
  await assert.rejects(toolsAt(f as any, "http://api", "t", "ws-1"), /is Stopped; start it/);
});

test("a rotated workspace token refetches the address once and retries once", async () => {
  let fetches = 0, refetched = 0;
  const f = async (_u: string, init: any) => (fetches++, init.headers.authorization === "Bearer new" ? res(200, { output: "ok" }) : res(401, { error: "expired" }));
  const getAt = async (fresh?: boolean) => (fresh && refetched++, { address: "10.0.0.1:7788", token: fresh ? "new" : "old" });
  const r = await callTool(f as any, getAt, "ws-1", "read", { path: "a" });
  assert.deepEqual(r.ok, true);
  assert.equal(refetched, 1);
  assert.equal(fetches, 2);
});

test("a refused connection says unreachable and start it", async () => {
  const f = async () => { throw new TypeError("fetch failed"); };
  const r = await callTool(f as any, async () => ({ address: "x:7788" }), "ws-1", "read", {});
  assert.equal(r.ok, false);
  assert.match((r as any).error, /ws-1 is unreachable.*start it/);
});

test("a tool error body passes through as the error", async () => {
  const f = async () => res(400, { error: "path escapes home" });
  const r = await callTool(f as any, async () => ({ address: "x:7788", token: "t" }), "ws-1", "read", {});
  assert.deepEqual(r, { ok: false, error: "path escapes home" });
});
```

- [ ] **Step 2:** `cd bench/mod && node --test lib/` fails.
- [ ] **Step 3: Implement `lib/platform.ts`.** Every call sends
  `authorization: Bearer <token>` and `content-type: application/json`. A 2xx tool body is
  serialised as `text` (string stays a string, an object becomes `JSON.stringify(body, null, 2)`).
  The token for `/v1` is read from `KL_TOOL_TOKEN_FILE` on every call, never cached (the same
  reason as `crates/ide/src/auth.rs`'s header).
- [ ] **Step 4: Implement `hooks/register.tsx`** with the signatures recorded in `spike.md`:
  - `session.start`: if `KL_WORKSPACE` is unset, register no tools (main session). Otherwise
    `at = await toolsAt(...)`, `for t of await listTools(...)` then
    `$.tool.register({name: t.name, description: t.description, input: t.input_schema, handler: input => callTool(...)})`.
    Wrap it in `.catch` so a failure logs `$.ui.log('kloudlite: no workspace tools: ' + e)` and
    registers nothing.
  - `tool.check`: if `DISABLED.includes(e.tool)`, deny with
    `"this bench runs no local tools; use the workspace's tools"`.
  - `tool.call {tool:'Agent'}` with no `e.agentId` and no `KL_WORKSPACE`: deny, using the demo's
    text verbatim (`register.tsx` in the demo).
  - `prompt.context`: if `KL_WORKSPACE` is set, return `workspaceContext(...)`.
  - If `spike.md` says "unreadable": `ui.render` for `ToolUse` and `ToolResult` on `edit`/`patch`
    draws `<Code language="diff">`, and on `exec` draws the command plus the last 40 output lines.
- [ ] **Step 5:** `node --test lib/` passes. `claude plugin validate bench/mod` exits 0.
- [ ] **Step 6:** Local smoke (Review Focus 5). With `KL_WORKSPACE=ws-x KL_API_URL=http://127.0.0.1:1`,
  run `claude -p --plugin-dir bench/mod --disallowedTools "<DISABLED>" "read README.md"`. The
  output must show no file read and must name the unreachable API. Paste the last 5 lines into the
  report.
- [ ] **Step 7:** Commit: `Register a workspace's tools as the bench agent's own tools`.

### Task 4: The kloudlite mod: the pane on real data

**Files:**
- Create: `bench/mod/lib/view.ts` (pure: `/v1` plus `/state` combined into the view model).
- Modify: `bench/mod/hooks/register.tsx` to port the demo's pane (the demo `register.tsx`
  `ui.render Pane`, list view and detail view, `/kloudlite`, `/env`).
- Test: `bench/mod/lib/view.test.ts`.

**Interfaces:**
- `buildView(workspaces: V1Workspace[], envs: V1Environment[], state: SessionsState): {workspaces: Row[], environment: EnvRow|null}`.
  `Row = {id, name, status: 'idle'|'running'|'errored'|'stopped', doing: string, queued: number, intercepts: string[], agents: {id,label,status}[]}`.
- Data sources (poll every 2 s with `$.clock.every`):
  - `GET {api}/v1/workspaces`;
  - `GET {api}/v1/environments/{selected}`;
  - `GET http://127.0.0.1:8917/state`.
- Sending goes to `POST http://127.0.0.1:8917/send {ws, text}`, or `{ws, agentId, text}` for a
  subagent. This replaces the demo's `$.session.send`, because subagents now live in
  `kl-sessions`.
- TASKS: `callTool(..., 'process_list', {})`. Kill: `callTool(..., 'process_kill', {id})`.

- [ ] **Step 1: Write the failing tests:**

```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import { buildView } from "./view.ts";

test("intercepts show as svc:port on the intercepting workspace", () => {
  const v = buildView(
    [{ id: "ws-1", name: "api", phase: "Running" }] as any,
    [{ id: "env-1", status: { services: [{ name: "web", ports: [8080] }], intercepted_by: { web: "ws-1" } } }] as any,
    {} as any,
  );
  assert.deepEqual(v.workspaces[0].intercepts, ["web:8080"]);
});

test("a session error line marks the row errored", () => {
  const v = buildView([{ id: "ws-1", name: "api", phase: "Running" }] as any, [],
    { "ws-1": { lines: ["u:x", "s:(error) boom"], busy: false, queued: [], agents: [] } } as any);
  assert.equal(v.workspaces[0].status, "errored");
});

test("a stopped workspace is stopped whatever the session says", () => {
  const v = buildView([{ id: "ws-1", name: "api", phase: "Stopped" }] as any, [], {} as any);
  assert.equal(v.workspaces[0].status, "stopped");
});
```

  Before writing `V1Workspace`/`V1Environment`, read the real response shapes from
  `crates/workspaces/src/api/workspaces/mod.rs` (list handler) and the environments GET handler,
  and use their field names (the test's `phase` and `intercepted_by` follow CLAUDE.md; correct
  both the test and the type if the JSON differs).
- [ ] **Step 2:** The tests fail. **Step 3:** Implement `view.ts`. **Step 4:** Port the pane from
  the demo, replacing the mock arrays with `buildView` output. **Step 5:** `node --test lib/` and
  `claude plugin validate bench/mod` pass.
- [ ] **Step 6:** Commit: `Show real workspaces, sessions and intercepts in the bench pane`.

### Task 5: Credentials: per-user provider token and a beat-minted bench-tool token

**Files:**
- Modify: `crates/core/src/jwt.rs` to add `mint_provider(&self, owner: &str, ttl: u64) -> Result<String>`
  (claims `{sub, jti, iat, exp, typ: "provider"}`) and `verify_provider(&self, tok) -> Result<ProviderClaims>`,
  next to `mint_registry`.
- Modify: `crates/workspaces/src/k8s/secrets.rs` so `user_key_secret` takes
  `provider_token: &str` and writes key `"provider-token"`.
- Modify: `crates/workspaces/src/api/workspaces/keys.rs:157-190` to mint
  `s.jwt.mint_provider(owner, 86_400)` beside `registry_token` and pass it in. Then, for every
  bench Workspace in `ns` (`crd::is_bench`) whose member is not Paused (reuse the Paused check
  `my_bench` uses in `api/bench.rs`), apply `crate::k8s::bench_tool_secret(ns, token, exp)` with
  `s.jwt.mint_bench_tool(owner, team, bench_id, bench_id)` and
  `WORKSPACE_TOOL_TTL_SECS`-scale expiry (R2). Check `mint_bench_tool`'s TTL parameter; if it is
  hard-wired to `BENCH_TOOL_TTL_SECS`, add a `ttl` argument.
- Modify: `crates/workspaces/src/api/bench.rs` to delete `mint_tool_token` and `revoke_tool_token`
  (lines 397-451) and their routes at `api/mod.rs:448-455`. Remove `/v1/bench/tool-token` from
  `NOT_BENCH_TOOL_ROUTES`. Keep `delete_tool_secret` (stop still calls it).
- Test: `crates/core/src/jwt.rs` unit test and `crates/workspaces/tests/api_bench.rs`.

- [ ] **Step 1: Write the failing tests:**

```rust
// crates/core/src/jwt.rs, mod tests
#[test]
fn a_provider_token_verifies_and_is_not_a_registry_token() {
    let j = test_jwt();
    let t = j.mint_provider("alice", 86_400).unwrap();
    assert_eq!(j.verify_provider(&t).unwrap().sub, "alice");
    assert!(j.verify_registry(&t).is_err(), "typ keeps audiences apart");
}
```

  Use the module's existing test constructor in place of `test_jwt()`, and the existing registry
  verify fn name. In `api_bench.rs`, modelled on the existing tool-token tests in that file:
  `the_keys_beat_writes_a_bench_tool_secret_for_the_bench` (run one beat with a bench in the
  namespace and assert Secret `bench-tool` exists with key `token` that `verify_bench_tool`
  accepts), and `a_paused_member_gets_no_bench_tool_secret`. Replace the tests of the deleted
  routes with `the_desktop_tool_token_route_is_gone` (`POST /v1/bench/tool-token` gives 404).
- [ ] **Step 2:** `cargo test -p kloudlite-core jwt` and
  `cargo test -p kloudlite-workspaces --test api_bench` fail.
- [ ] **Step 3:** Implement. **Step 4:** Both pass, plus clippy.
- [ ] **Step 5:** Commit: `Mint the provider and bench tool tokens on the keys beat`.

### Task 6: The bench pod and image

**Files:**
- Create: `bench/sv/{tmux,ttyd,sshd,sessions}/run` (sh, `exec` last). The four bodies:
  - `tmux`: `exec tmux -D new -A -s kl claude` (`-D` keeps tmux in the foreground, so runsv
    supervises it).
  - `ttyd`: `exec ttyd -W -i 0.0.0.0 -p 7681 tmux attach -t kl`.
  - `sshd`: `mkdir -p ~/.ssh-host && [ -f ~/.ssh-host/ed25519 ] || ssh-keygen -q -t ed25519 -N '' -f ~/.ssh-host/ed25519; exec /usr/sbin/sshd -D -e -f /etc/kl/sshd_config`.
  - `sessions`: `exec node /opt/kl/sessions/main.ts`.
- Create: `bench/sshd_config` with `Port 7789`, `HostKey /home/kl/.ssh-host/ed25519`,
  `AuthorizedKeysFile /etc/kloudlite/authorized_keys`, `PasswordAuthentication no`,
  `UsePAM no`, `PidFile none`, and `ForceCommand tmux new -A -s kl claude`.
- Create: `bench/settings.json` (managed settings, copied to `/etc/claude-code/managed-settings.json`)
  containing `{"apiKeyHelper": "cat /etc/kloudlite/ssh/provider-token"}`. Confirm the mount path
  in `USER_KEY_PATH`.
- Rewrite: `deploy/bench/Dockerfile`: base `node:24-bookworm-slim`; apt `ca-certificates runit tmux ttyd openssh-server`;
  `npm i -g @anthropic-ai/claude-code@<exact version>`; `COPY bench/sessions` then `npm ci --omit=dev`
  in `/opt/kl/sessions`; `COPY bench/mod /opt/kl/mod`; `COPY bench/sv /etc/kl/sv`;
  `COPY bench/sshd_config /etc/kl/`; `COPY bench/settings.json /etc/claude-code/managed-settings.json`;
  keep the `kl` binary lines; `ENV CLAUDE_CODE_PLUGIN_DIRS=/opt/kl/mod`; `USER 1000:1000`.
  Write the header comment anew, saying why each package is there.
- Modify: `deploy/dev/pod/ship.sh:149-154` to copy `bench/` into `$CTX`, and `.dockerignore` to
  admit `bench/`.
- Modify: `crates/workspaces/src/k8s/bench.rs`:
  - command `["runsvdir", "/etc/kl/sv"]`;
  - env removes `PI_CODING_AGENT_DIR`, `KL_MODEL`, `OTEL_SERVICE_NAME="harness-bench"` and adds
    `ANTHROPIC_MODEL` (from `spec.bench.model`, if set), `ANTHROPIC_BASE_URL` (new param
    `provider_url`, omitted when empty) and `KL_BENCH_IDLE_SECS`;
  - adds the owner `authorized_keys` hostPath mount (copy the workspace pod's volume and mount
    from `k8s/workspace.rs`, mounted at `/etc/kloudlite/authorized_keys`);
  - ports 7789 `ssh` and 7681 `ttyd` (add `pub const BENCH_TERM_PORT: u16 = 7681;`);
  - both probes exec `["node", "/opt/kl/sessions/main.ts", "--ping"]`;
  - the header doc is updated (runit, four services, the R3/R4 reasons).
- Modify: `crates/workspaces/src/crd/settings.rs` to add `bench_provider_url: Option<String>`
  (default `""`, META `("benchProviderUrl", Mark::Boot, &[])`, doc: "the base URL bench agents
  send model requests to; the token is the user's `provider-token`"). Thread it through
  `workspace.rs:637` and `bins/agent/src/controller/workspace/mod.rs:426` in Kompress's old slot.
- Modify: `allow_gateway_bench` in `k8s/bench.rs:190` to also admit `BENCH_TERM_PORT`.
- Test: `crates/workspaces/src/k8s/tests/bench.rs`.

- [ ] **Step 1: Write the failing tests** in `k8s/tests/bench.rs`:

```rust
#[test]
fn a_bench_runs_its_services_under_runit_and_pings_kl_sessions() {
    let c = bench_container_for_test(); // the file's existing builder, provider_url "https://llm.example"
    assert_eq!(c.command.as_deref(), Some(&["runsvdir".to_string(), "/etc/kl/sv".into()][..]));
    let probe = c.readiness_probe.unwrap().exec.unwrap().command.unwrap();
    assert_eq!(probe, ["node", "/opt/kl/sessions/main.ts", "--ping"]);
    let env = |k: &str| c.env.as_ref().unwrap().iter().find(|e| e.name == k).and_then(|e| e.value.clone());
    assert_eq!(env("ANTHROPIC_BASE_URL").as_deref(), Some("https://llm.example"));
    assert!(env("ANTHROPIC_API_KEY").is_none() && env("ANTHROPIC_AUTH_TOKEN").is_none(),
        "the token is read by apiKeyHelper from the mounted file, never stamped");
    let ports: Vec<i32> = c.ports.unwrap().iter().map(|p| p.container_port).collect();
    assert_eq!(ports, [7789, 7681]);
}

#[test]
fn an_empty_provider_url_stamps_no_base_url() { /* same builder with "" — env("ANTHROPIC_BASE_URL") is None */ }

#[test]
fn a_bench_mounts_the_owners_authorized_keys() { /* assert a mount at /etc/kloudlite/authorized_keys, read_only */ }
```

  Fill the two short bodies in the same style as the first test.
- [ ] **Step 2:** The tests fail. **Step 3:** Implement the Rust. **Step 4:** Tests and clippy
  pass.
- [ ] **Step 5:** Build the image locally to prove the Dockerfile:
  `docker build -f deploy/bench/Dockerfile .` after building the musl `kl` the Dockerfile copies.
  If the laptop cannot build musl, comment out the `kl` COPY for this check only (do not commit
  that) and record it. Then
  `docker run --rm --entrypoint sh <img> -c 'runsvdir -h; tmux -V; ttyd --version; claude --version; node /opt/kl/sessions/main.ts --ping; echo $?'`.
  Expected: versions print, and the ping exits 2 (no server).
- [ ] **Step 6:** Commit: `Run the agent CLI, ttyd, sshd and the session service in the bench pod`.

### Task 7: `kl-connect bench` over SSH

**Files:**
- Modify: `bins/kl-connect/src/bench.rs`. It keeps binding the local port and pumping to the
  gateway (unchanged), then `exec`s
  `ssh -p <port> -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=~/.config/kloudlite/bench_known_hosts -t kl@127.0.0.1`
  as a child and exits with its status. Copy the ssh invocation style from kl-connect's workspace
  ssh command (`grep -rn "Command::new(\"ssh\")" bins/kl-connect/src`).
- Test: `bins/kl-connect/src/bench.rs` unit test of the argv builder `fn ssh_argv(port: u16, known_hosts: &Path) -> Vec<String>`.

- [ ] **Step 1:** Write a failing test asserting the argv above exactly. **Step 2:** It fails.
  **Step 3:** Implement. **Step 4:** `cargo test -p kl-connect` and clippy pass.
- [ ] **Step 5:** Commit: `Open the bench over SSH from kl-connect`.

### Task 8: The browser terminal through the gateway

**Files:**
- Create: `bins/gateway/src/term.rs`, an HTTP and WebSocket reverse proxy at `/term/{bench}/{*rest}`.
  - Auth: the bench-session token arrives as `?token=` on the first GET. The gateway verifies it
    (`verify_bench_session`, `claims.bench == {bench}`) and sets the cookie
    `kl_term=<token>; Path=/term/{bench}/; HttpOnly; Secure; SameSite=Strict`. Later requests
    verify the cookie.
  - `// ponytail: the 60 s session token bounds page load + WS connect; a reconnect re-opens from the console; mint a longer term token if reconnects annoy.`
  - Target: `resolve_bench(client, bench, BENCH_TERM_PORT)`.
  - HTTP: forward GET with path `/{rest}` over a plain `tokio::net::TcpStream` HTTP/1.1 request
    (ttyd serves only `/`, `/token`, `/ws` and static files), or with `hyper` if the workspace
    already has it (`cargo tree -p kloudlite-gateway | grep hyper`).
  - WS: an axum `WebSocketUpgrade` on the client side, and `tokio-tungstenite` on the ttyd side
    (already a dev-dep, move it to deps) with subprotocol `tty`. Pump frames both ways.
- Modify: `bins/gateway/src/tunnel.rs:189` `app()` to `.merge(term::routes(gw))`.
- Create: web `web/apps/web/app/(app)/bench/page.tsx` (follow the sibling route group layout;
  `ls web/apps/web/app`), a server action calling `POST /v1/bench/session`, and rendering
  `<iframe src={`${https gateway}/term/${id}/?token=${token}`} className="h-[calc(100vh-…)] w-full border-0" />`.
  Turn `gateway` (`wss://…`) into `https://…`. Handle 202 `{state}` with a "starting your bench…"
  placeholder and a refresh every 2 s. Use tokens, not raw colors.
- Test: `bins/gateway/tests/term.rs` (model on the existing gateway tests' fake resolver), and the
  web `bun test` for the URL helper.

- [ ] **Step 1: Write the failing tests:**
  - `a_term_request_without_a_token_is_401`;
  - `a_token_for_another_bench_is_403`;
  - `the_first_request_sets_the_cookie_and_proxies` (fake upstream on a local port answers `200 ttyd`);
  - `a_websocket_is_pumped_both_ways` (fake upstream ws echo);
  - web: `termUrl("wss://gw.example", "bench-1", "t")` equals
    `"https://gw.example/term/bench-1/?token=t"`.
- [ ] **Step 2:** They fail. **Step 3:** Implement. **Step 4:** `cargo test -p kloudlite-gateway`,
  clippy, and `cd web && bun run lint && bun run typecheck && bun run test` pass.
- [ ] **Step 5:** Commit: `Serve the bench terminal in the browser through the gateway`.

### Task 9: SLO probes

**Files:**
- Modify: `crates/workspaces/src/slo/catalogue.rs`:
  - remove `bench.session.roundtrip` (627), `bench.two_clients` (628), `bench.tool.revoked` (631),
    and `bench.shell.workspace`/`bench.delegate` if their stage code calls the deleted harness
    server (check `bins/slo/src/stages/bench_ws.rs`);
  - reword the `bench.tunnel` (358) and `bench.idle.wake` (626) SLIs to the spec's table;
  - add `bench.claude.tool_roundtrip` and `bench.builtin.refused` (feature "Benches", suite Hourly,
    stage "14 · Experience", target `bound(120_000)` and `avail(99.9)`);
  - update the id list at 166-177 and `every_bench_id_is_catalogued` (784-790).
- Modify: `deploy/slo.md` rows to match (the existing equality test enforces it).
- Modify: `bins/slo/src/stages/bench.rs`, `bench_tool.rs` and `bench_ws.rs`:
  - `bench.tunnel`: open the tunnel and read the SSH banner (`SSH-2.0-`) from port 7789 through it;
    then `GET {gateway}/term/{id}/?token=` gives 200.
  - `bench.claude.tool_roundtrip` (and `bench.builtin.refused`): there is no client shell, so drive
    them through SSH. Over the tunnel, run
    `ssh … kl@ 'curl -s -XPOST 127.0.0.1:8917/send -d {"ws":"<probe ws>","text":"Use the read tool on ~/workspace/README.md and quote its first line."}'`.
    That needs the probe's key in the owner's authorized keys; reuse how `bench_ws.rs` gets its SSH
    identity. If that path cannot work, stop and report rather than inventing one. Then poll
    `/state` until not busy (≤ 120 s). Pass when there is a `t:read(` line and an `r:` line
    containing the file's first line.
  - `bench.builtin.refused`: send `"Run the Bash tool with: echo hi"`. Pass when no `r:hi` appears
    and a line contains `runs no local tools`.
  - `bench.tool.token`/`audience` mint the bench-tool token with the probe's `kloudlite-jwt`
    (R2) in place of the deleted route.
- [ ] **Step 1:** Edit the catalogue and `slo.md` so the equality test is the failing test.
  **Step 2:** Implement the stages. **Step 3:** `cargo test -p kloudlite-workspaces slo`,
  `cargo test -p slo` and clippy pass.
- [ ] **Step 4:** Commit: `Probe the bench agent through its sessions, not the retired server`.

### Task 10: Remove dead bench references

- [ ] **Step 1:** Run `grep -rn "harness-bench\|harness/bench\|sys-1\|sys1\|PI_CODING\|bench\.ts\|/sessions/{id}/messages" crates bins deploy web/apps/web --include='*.rs' --include='*.ts' --include='*.tsx' --include='*.md' --include='*.yaml' --include='*.sh'`.
  Fix every live reference: delete it, or reword a `//!` header to the new design. Shipped design
  docs under `docs/` stay as history.
- [ ] **Step 2:** Full gate:
  `CARGO_TARGET_DIR=/Volumes/kdisk/target-sys1-wire cargo clippy --workspace --all-targets -- -D warnings && cargo test`,
  then `node --test` in `bench/sessions` and `bench/mod`, then the web gate. Report pass/fail with
  only the failing lines.
- [ ] **Step 3:** Commit: `Drop the last references to the sys-1 bench`.
