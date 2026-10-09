# Roles and task board

**Goal:** Two roles only. Main orchestrates: it creates, clones, starts and deletes workspaces, keeps a
task board and hands work out with `workspace_ask`. A workspace session works on its own workspace:
it reports to main with `main_tell` and stops itself with `workspace_stop`. The TUI shows each
workspace's current task and its queue.

**Owner words (2026-10-09):** "I actually don't want workspace/subagents to be able to create new
workspaces, or delete existing. they can stop their own after finishing." / "they will be able to
communicate with main orchestrator once the tasks are done or they need anything else orchestrated
across." / subagent question answered "Main creates them" / "I want you to show what is current task
each workspace or subagent is working on, along with the queue. and based on the priority and
dependency the tasks can be rearranged. things will be orchestrated by main working session." /
"I want you to fix all these" (every vetting finding).

**Defaults taken (owner told):** the board shows in the TUI only; main deletes finished clones.

**Facts:** scratchpad `redesign-facts.md`, `role-context-map.md`. Paths below are under `harness/`.

## Global constraints

- Run checks with `cd harness && bun run check && bun test`. Both must pass.
- No server change: the bench token already may `POST /v1/workspaces/{id}/stop`.
- No auto-dispatcher. Main decides what runs next; the board only records and orders.
- Commit messages: imperative sentence case, no tool or model names, no trailers.
- Comments explain WHY, matching the surrounding files.

## Review focus

1. A workspace answers an ask AND calls `main_tell done`: main must get one report, not two
   (`reported` suppression, test in T1).
2. A `main_tell` from a workspace whose main session is not open (bench restarted): it opens main
   and delivers (deliver path is the ask's; test in T1).
3. `workspace_ask` naming a task whose deps are not done: refused with the blocking ids, nothing
   dispatched (test in T2).
4. `task_update` making a dependency loop or naming an unknown task: refused, board unchanged
   (test in T2).
5. A resumed session (messages already on disk) must not get the role card a second time (test in T1).

---

### Task 1: Remove subagent, trim main, self-stop, main_tell, role card

**Files**
- Modify: `packages/backend/src/delegate.ts`, `packages/backend/src/local.ts`,
  `packages/backend/src/asks.ts`, `packages/backend/src/forget.ts` (header comment only),
  `packages/tools/src/platform.ts`, `packages/agent/src/claude.ts`, `packages/agent/src/index.ts`,
  `apps/tui/src/sessions.ts`, `apps/tui/src/app.tsx`, `apps/tui/src/workspaces.ts`
- Delete: `packages/agent/claude-plugin/skills/subagent-session/`
- Tests: `packages/backend/src/delegate.test.ts`, `packages/backend/src/local.test.ts`,
  `packages/agent/src/index.test.ts`, any `apps/tui` test that asserts `:agent-` behaviour

**1a. Subagent goes.**
- delegate.ts: delete `runInClone` and every helper only it uses (`sq`, `tail`, `MOVED`,
  `commitCmd`, the init/start command helpers, `WS` alias if unused; keep `hex` and
  `WORKSPACE_DIR`). Delete `DelegateDeps.api`, `exec`, `sleep`, `forget` and any import left unused
  (`api`, `podExec`, `ExecResult`, `forgetSessions`). Delete the `subagent` ToolDef. Rewrite the
  `//!` header: `workspace_ask` (main to workspace) and `main_tell` (workspace to main); drop every
  subagent sentence.
- local.ts: `SessionKind` loses the `subagent` variant; `sessionKind` returns
  `{ kind: "workspace", ws: base }` for every non-main key; `registryFor` loses its third branch;
  `sessions.list` drops the `:agent-` filter (`list: async (prefix?) => listSessions(prefix)` if
  `listSessions` takes an optional prefix, else keep the ternary without the filter); the
  `registryFor` and `sessionCwd` comments lose subagent wording.
- agent: `Role = "main" | "workspace"` (claude.ts:49); index.ts comments at 141 ("workspace and
  subagent sessions" becomes "workspace sessions"), 362-363 and 405 lose subagent wording.
- Delete the skill dir `subagent-session`.
- TUI: `sessions.ts:36,57-61`, `app.tsx:281`, `workspaces.ts:143`: remove the "clone has only its
  subagent's session" handling. A clone row opens its own session key (the clone's id), like any
  workspace. Keep the clone indentation under its parent (`fromSpace`) as is.
- Tests: delete the subagent tests in delegate.test.ts (lines ~140-305: everything from the clone
  fakes through "no clone, no clone request", plus "main gets no subagent"); keep the
  workspace_ask tests and the permission-routing test, rewritten to use `workspace_ask` if it used
  `subagent`. Remove subagent assertions from local.test.ts and agent index.test.ts. Add:
  `test("delegateTools: main gets workspace_ask, a workspace gets main_tell", ...)`.

**1b. Main's tool list trims.** platform.ts main list:
`...["packages_list", "packages_add", "packages_remove", "packages_update"].map(of)` becomes
`of("packages_list")`, and `...["service_add", "service_update", "service_remove", "intercept",
"release"].map(of)` becomes `...["service_add", "service_update", "service_remove"].map(of)`.
`hello().tools` derives from `platformTools("main")`, so it follows; it lists main's tools only,
so `main_tell` is not added there.
Test (platform test file or local.test.ts): main's names exclude `packages_add`,
`packages_remove`, `packages_update`, `intercept`, `release`, `subagent`; include `packages_list`,
`workspace_create`, `workspace_delete`, `workspace_ask`.

**1c. `workspace_stop` for a workspace session, no card.**
- platform.ts workspace (`shared`) list, before the `if (ws) return shared;` split: add, only when
  `ws`:
  `def("workspace_stop", "Stop this workspace once its task is done and main or the person said to stop; it snapshots first, the next start resumes it." + ASYNC, {}, [], async () => api("POST", \`${W}/stop\`))`
  (use the same `W` base the other workspace tools use). Main keeps its own `lifecycle`
  `workspace_stop` with a `workspace` param; so add this one to the array returned for `ws` only
  (e.g. `if (ws) return [...shared, selfStop];`), never to `shared`, so `of()` is unchanged.
- local.ts: `mustAsk(name, fence?, egress = ..., kind: "main" | "workspace" = "main")`, first line
  `if (kind === "workspace" && name === "workspace_stop") return false;`.
  `installGate(agent, permission, fence?, words?, kind: "main" | "workspace" = "main")` passes
  `kind` to `mustAsk`. The call in `session()` passes `k.kind`.
  `registryFor` workspace branch maps `(t) => (t.name === "workspace_stop" ? t : asking(t))` so the
  self-stop carries no `because`.
- Tests: `mustAsk("workspace_stop", undefined, "open", "workspace") === false`,
  `mustAsk("workspace_stop", undefined, "open", "main") === true`; a workspace registry's
  `workspace_stop` schema has no `because` and no `workspace` property; main's still requires both.

**1d. `main_tell` (workspace sessions only).** In delegate.ts.
- `DelegateDeps` gains `lastCaller?: Map<string, string>` (ws key to the key of the session that last
  asked it) and `reported?: Set<string>` (ws keys that already sent done/blocked during the current
  ask). LocalBackend owns one of each (private fields) and returns them from `#deps()`.
- Generalise `deliver(deps, a, reply)` to
  `deliver(deps, to: string, o: Pick<SessionOpts, "model" | "thinkingLevel" | "autoCompact" | "codemode">, reply: string, from: string)`;
  the two log lines read `reply lost ${to} ${from}`. Ask callers pass `(a.callerKey, a, reply, a.key)`.
- `dispatchAsk`: first line after `saveAsk`: `deps.lastCaller?.set(a.key, a.callerKey); deps.reported?.delete(a.key);`.
  After the answer: `const told = deps.reported?.delete(a.key) ?? false;` then `dropAsk`; deliver the
  reply only when `!told`; dispose the view either way.
- The tool:
  ```ts
  const tell: ToolDef = {
    name: "main_tell",
    description:
      "Tell the main session something. done: the task is finished (tests pass, branch pushed); blocked: you cannot go on, say what would unblock you; need: a fact or an action from another workspace or the person, then keep working on what you can. Name the task id when the work came with one. After done or blocked, end your turn: your report is the answer.",
    inputSchema: {
      type: "object",
      properties: { kind: { type: "string", enum: ["done", "blocked", "need"] }, task: { type: "string" }, text: { type: "string" } },
      required: ["kind", "text"],
    },
    async run(i: { kind: "done" | "blocked" | "need"; task?: string; text: string }) {
      const to = deps.lastCaller?.get(ws!) ?? "main";
      let msg = `[from ${ws}]${i.task ? ` [task ${i.task}]` : ""} ${i.kind}: ${i.text}`;
      if (i.kind !== "need") {
        deps.reported?.add(ws!);
        msg += boardNote(deps, ws!, i);   // Task 2 fills this; T1 ships `const boardNote = () => ""`
      }
      await deliver(deps, to, caller, msg, ws!);
      return `told ${to}`;
    },
  };
  if (kind === "workspace") return [tell];
  ```
- Tests (delegate.test.ts, with the existing fakes):
  - `main_tell` delivers `[from ws-a] [task T1] need: x` into `main` (prompt when idle, followUp
    when busy).
  - It goes to `lastCaller` when set (`main:2`), else `main`.
  - With main not in `live`, `open("main", …)` is called and the message is prompted.
  - An ask whose session calls `main_tell done` during the turn delivers exactly one message to
    the caller (the tell), not the final answer; the next ask to the same workspace delivers its
    answer again (reported cleared).
  - `need` does not suppress the final answer.

**1e. Role card on the first prompt.** local.ts `baseHandle`:
`prompt: async (text, o) => agent.prompt(agent.messages.length === 0 ? \`${roleCard(key)}\n\n${text}\` : text, o)`.
`roleCard` (exported, local.ts):
```ts
/** Who this session is, on its first message: Claude sessions see skills only by name until they
 * load one, so the role must arrive in the conversation itself (the system prompt stays Claude
 * Code's own for billing, claude.ts). */
export function roleCard(key: string): string {
  const k = sessionKind(key);
  if (k.kind === "main")
    return [
      "[role: main session]",
      "You orchestrate. You create, clone, start and delete workspaces and environments, keep the task board (task_add, task_update, task_list), and hand work to a workspace with workspace_ask (pass the task id).",
      "You never write the code yourself. Reports arrive as `[from <ws>] ...` messages; update the board from them and dispatch the next ready task.",
    ].join("\n");
  return [
    `[role: workspace session for ${k.ws}]`,
    "You work only in this workspace. Work comes from the person or as `[from main session] [task T<n>] ...`.",
    "You never create, clone, restore or delete workspaces. Report to main with main_tell: done, blocked, or need (for a fact or action from another workspace).",
    "Stop this workspace with workspace_stop only after you finished and main or the person said to stop.",
  ].join("\n");
}
```
Tests: a fresh fake agent's first `prompt("hi")` reaches the agent as text starting `[role: main session]`
and ending `hi`; a second prompt arrives unchanged; an agent whose `messages` already has one entry
(resumed) gets `hi` unchanged; a workspace key `ws-a` gets `[role: workspace session for ws-a]`.
`answer()` in delegate.ts matches the asked message with `includes`, so asks still resolve; keep
the existing ask tests green as proof.

**Commit:** `Make main the only orchestrator and give workspaces main_tell`.

---

### Task 2: Task board

**Files**
- Create: `packages/backend/src/tasks.ts`, `packages/backend/src/tasks.test.ts`
- Modify: `packages/backend/src/index.ts` (types), `packages/backend/src/local.ts`,
  `packages/backend/src/delegate.ts`, `packages/backend/src/asks.ts`,
  `apps/tui/src/components/Sidebar.tsx`, `apps/tui/src/app.tsx` (pass tasks), a new pure helper
  `apps/tui/src/tasks.ts` + `apps/tui/src/tasks.test.ts`

**Types (index.ts, beside SpaceView):**
```ts
export type TaskState = "queued" | "running" | "blocked" | "done" | "failed";
export type BoardTask = { id: string; title: string; workspace?: string; priority: number; dependsOn: string[]; state: TaskState; note?: string; created: number };
```
`SpaceView` gains `tasks?: BoardTask[]` (optional: the remote backend and fixtures need no change).

**Store (`tasks.ts`, `//!` header: main's board, one file, written by main's tools and by
`main_tell`/`workspace_ask`; no dispatcher by design).** Write pattern copied from asks.ts (tmp file
+ rename).
```ts
export const tasksFile = () => join(homedir(), ".kl", "tasks.json");
export function readTasks(file: string): BoardTask[]            // missing/unparsable file = []
export function addTask(file, a: { title: string; workspace?: string; priority?: number; dependsOn?: string[]; note?: string }): BoardTask | string
export function updateTask(file, id: string, p: Partial<Pick<BoardTask, "title" | "workspace" | "priority" | "dependsOn" | "state" | "note">>): BoardTask | string
export function ready(ts: BoardTask[], workspace?: string): BoardTask[]
export function blockers(ts: BoardTask[], t: BoardTask): string[]   // deps not done
export function boardText(ts: BoardTask[]): string
export function taskTools(file: string): ToolDef[]                  // task_add, task_update, task_list
```
Rules:
- ids `T1`, `T2`, … (max existing number + 1). `priority` default 3, lower runs first.
  `created = Date.now()`.
- `dependsOn` entries must exist and must not reach back to the task (DFS over `dependsOn`);
  errors are strings: `error: unknown task T9`, `error: T2 -> T3 -> T2 is a loop`. Board unchanged on error.
- `ready(ts, ws?)`: `state === "queued"`, `blockers` empty, `workspace` equal to `ws` when given
  (unassigned tasks count for `ws === undefined` only), sorted by `priority` then `created`.
- `boardText`: one line per non-done task, grouped by workspace then `unassigned`:
  `T3 [running] p2 fix login (ws-a)  waits on T1` (the `waits on` part only when blockers exist),
  then `N done` on the last line. `task_list` returns it; `"no tasks"` when empty.
- Tools (main only, not gated):
  - `task_add {title, workspace?, priority?, depends_on?: string[], note?}` returns the task line.
  - `task_update {id, title?, workspace?, priority?, depends_on?, state?, note?}`; `state` enum the five states.
  - `task_list {}`.
- `DelegateDeps` gains `tasks?: string` (file path; tests pass a temp file). LocalBackend passes
  `tasksFile()`.

**Wiring**
- local.ts `registryFor` main branch adds `...taskTools(deps.tasks ?? tasksFile())`; `hello().tools`
  adds `"task_add", "task_update", "task_list"`.
- `LocalBackend.space`: `space = async () => ({ ...(await space()), tasks: readTasks(tasksFile()) })`
  (rename the import if it shadows).
- `workspace_ask` gains `task?: string`. When given: read the board; unknown id is
  `error: unknown task T9`; blockers non-empty is `error: T3 waits on T1, T2` (nothing dispatched);
  else `updateTask(file, id, { state: "running", workspace: key })` and the text is
  `[from main session] [task T3] ${request}`. `PendingAsk` gains `task?: string` (asks.ts) so a resend
  keeps it. When the ask's final answer is delivered (not suppressed) and `a.task` is set and the
  task is still `running`, append `\n(T3 is still running on the board; task_update it)`.
- `main_tell` `boardNote` (replaces T1's stub): when `i.task` is set and exists,
  `updateTask(file, i.task, { state: i.kind === "done" ? "done" : "blocked", note: i.text.slice(0, 200) })`;
  return `\nboard: T3 ${state}` + (next ready for this ws: `; next for ws-a: T5 title`) + (tasks that
  became ready because this one is done: `; now ready: T6, T7`). Unknown task id: `\nboard: no task T9`.
  No task id: `""`.

**Tests (`tasks.test.ts`, temp file per test):** add assigns T1/T2; unknown dep refused; loop via
update refused and file unchanged; ready orders by priority then created and skips blocked-by-deps;
ready per workspace; boardText shows `waits on`; a missing file reads as `[]`. delegate.test.ts:
`workspace_ask` with a blocked task returns the error and opens nothing; with a ready task marks it
running on ws and the text carries `[task T1]`; `main_tell done` with task marks it done and the
delivered text names the next ready task for that workspace and the newly ready dependants.

**TUI.** `apps/tui/src/tasks.ts`:
```ts
export type TaskGroup = { workspace?: string; current?: BoardTask; queue: (BoardTask & { waits: string[] })[] };
export function taskGroups(ts: BoardTask[]): { groups: TaskGroup[]; done: number }
```
One group per workspace that has tasks (sorted by workspace name), then one `workspace: undefined`
group for unassigned. `current` = the running task, else the blocked one. `queue` = the rest of the
non-done, non-failed tasks of that group sorted by priority then created, each with `waits` = ids
of its deps not done. Failed tasks are listed in `queue` with their state. Test it.
Sidebar.tsx: a `Tasks` section under the existing ones, using its `Heading`/`Row` components:
per group a row with the workspace name (`unassigned` for none), the current task as
`▸ T3 fix login` (`blocked` in the warning color when blocked), queue rows indented as
`T5 add tests` plus ` waits on T3` dimmed; a final dim `N done` row when N > 0; the section is
hidden when there are no tasks. app.tsx: the 5 s space poll already holds the `SpaceView`; pass
`view.tasks ?? []` to the Sidebar.

**Commit:** `Add a task board main keeps and the sidebar shows`.

---

### Task 3: Skills (written by the controller, not an implementer)

Rewrite `packages/agent/claude-plugin/skills/{kloudlite,main-session,workspace-session,codemode}/SKILL.md`:
two roles; the role table; delete example under main only; cross-session = `workspace_ask` +
`main_tell`; main's board routine with task ids; parallel work by clone (main clones, asks the clone
"you are a clone of X for task T; push task branch B to origin; report with main_tell; stop"),
landing by asking the parent to merge branch B, deleting finished clones; workspace: `main_tell`,
`workspace_stop`, `service_logs`, a clone section, `question` only when the person is in view else
`main_tell need`, the person's direct words win (tell main on conflict), report shape (status, task
id, branch and commit, facts for others, decisions for the person), done checklist (tests pass,
branch pushed, service left as asked, intercept released unless told otherwise), `need` instead of
guessing another component's facts, AGENTS.md test command; codemode: no subagent. Fold in the held
integration-test sections from the dirty master edits, then drop those edits.

**Commit:** `Rewrite the role skills for main and workspace sessions`.
