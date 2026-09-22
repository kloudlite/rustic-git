# sys-1 sessions: design

Date: 2026-09-22. Supersedes `2026-09-22-sys1-engine-design.md` (the `RpcChild` seam), which was
never approved.

## Goal

Replace the pi session manager in `harness/bench` with the sys-1 engine (common term: "system one
model"), copied in as source from `~/dev/jevharn/src`, never used as a library, the pi packages
dropped. Sessions become a tree of three tiers, every session runs on the bench in one process,
and every state change is durable on the bench folder before it is acknowledged.

## Owner rulings

1. One workspace per session, one engine per session.
2. Every engine runs on the bench pod, in one process, no child processes. Workspace pods run only
   `kl ide serve`; an engine acts on its workspace over `/tools` on port 7788.
3. Three tiers: **top** (no source, talks to the person, delegates to mains), **main** (one per
   architecture component, long-lived, orchestrates only, never edits, the only tier that pushes
   to the platform repos), **sub** (spawned by a main, drives a clone of main's workspace).
4. A sub's result goes by `git push` from the clone pod straight into the main workspace's repo,
   onto main's working branch. No platform repo in between.
5. After the push the clone is deleted; the sub session is closed and kept read-only.
6. Delegation is a message into the child session; the parent never waits. The child's answer
   arrives later as a message into the parent. Parents run children in parallel.
7. Sessions persist on the bench's own folder; on restart rebuild the tree, mark a mid-flight turn
   interrupted, never resume it automatically.
8. The operation executor stays the platform-mutation layer (create, clone, delete, push, spawn go
   through it with its approvals and durability). The engine replaces only pi.

## Facts the design rests on

- The bench is a `Workspace` of kind bench with no btrfs volume. `/bench` is a hostPath on the
  region NFS share, `{pool}/homes/.benches/{team}/{owner}` (`crates/workspaces/src/k8s/bench.rs`).
  It outlives a pod restart, the idle exit, and node death. `.lock` fences one writer.
- Port 22 between a main pod and its clone is open today: `allow-same-namespace`
  (`crates/workspaces/src/k8s/policies.rs`) admits every port between pods of one namespace, and a
  clone lands in the owner's namespace beside its source.
- The clone pod cannot ssh into main today. Every pod mounts the owner's platform private key at
  `/etc/kloudlite/ssh/id_ed25519`, but `rotate_user_key` registers its fingerprint only in the git
  auth store, and `authorized_keys_for` (`crates/api/src/credentials.rs`) renders authorized_keys
  from directory `SshKey` credentials only. The fix is to append the platform public key there.
  Exposure does not change in practice: siblings already reach each other on every port and the
  bench already holds `exec` on 7788.
- The tool server (`crates/ide`) provides `read write edit patch glob grep exec process_list
  process_output process_write process_kill watch watch_poll watch_stop` plus the graft tools. The
  executor's workspace-touching tools map one to one: `read write edit glob grep` by name, `bash`
  to `exec`, `bash_output` and `kill_shell` to `process_output` and `process_kill`.
- A same-node clone answers 202 in about 0.3 s and is Ready about 3 s later.

## 1. Durable state model

Everything that matters is a file under `/bench`, written before it is acknowledged. Memory is a
cache rebuilt from disk on boot.

- `sessions.json`: the tree. The existing `SessionList` rows gain `parent` (seq), `tier`
  (`top | main | sub`), `state` (`open | closed`) beside the existing `workspace`.
- `sessions/{seq}.jsonl`: one append-only log per session, the whole conversation. Row kinds:
  `user` (an inbound message; `from` is the person, a parent seq or a child seq), `turn.start`,
  `turn.step` (every model call and reply, every tool call and result, so a partial turn is
  readable), `turn.end` (the final answer, or `error`), `delegate` (child seq, instruction),
  `interrupted`.
- The model context for a turn is built from these rows at the start of the turn and never
  carried across turns in memory.
- The inbox is derived, never stored: unread = `user` rows after the last `turn.end`.
- Delegation is two writes in order: the child's `user` row, then the parent's `delegate` row. A
  crash between leaves a child turn that still answers the parent later.
- A child's answer is two writes: the child's `turn.end`, then the parent's `user` row with
  `from: child`. A crash between is repaired on boot: a child whose newest `turn.end` has no
  matching parent `user` row (matched by child seq and turn index) is re-delivered.
- Boot: read `sessions.json`, scan each log's tail. A `turn.start` without a `turn.end` gets an
  `interrupted` row. Then the scheduler runs every session with unread rows. The interrupted turn
  itself is never resumed.

In memory only, all rebuildable: the `running` flag per session, the one model call in flight,
handles to processes a tool started (they live in the workspace pod and are found again by id),
and a parsed cache of the rows.

Skipped: fsync per row, add when NFS close-to-open shows a torn tail. Skipped: log compaction, add
when a log passes about 50 MB.

## 2. Session actor and scheduling

One `Session` object per row, all in the bench process. It holds its engine instance, its log
handle and a `running` flag. A turn is one async function; there are no threads.

- One scheduler loop runs on boot, on every inbound write and when a turn ends: for each open
  session with unread rows and `running == false`, start a turn. Parents and children run
  concurrently; one session never runs two turns at once.
- A turn appends `turn.start`, joins the unread `user` rows into one prompt (a parent that got
  three child answers sees them together), runs the engine, appends `turn.end`. When the answer
  belongs to a parent, the parent's `user` row is the next write.
- Tiers are engine deps, not prompt text: an allow-list of tools passed in `ActDeps`. Top:
  `delegate`, `ask_user`, `tell_user`, `think`, `recall`. Main: those plus the read-only workspace
  set (the executor's `readOnly` mode refuses changing tools) and `push`. Sub: everything.
- `delegate(target, instruction)` is a tool. For top, `target` names a main by workspace name;
  for main, `target` is empty (spawn a new sub) or an open child seq (answer that child). The
  parent's own answer comes back at once ("delegated, waiting") and its turn ends; the parent
  never waits.
- Abort stays per session as today; it appends `interrupted` and clears `running`.
- `MAX_RUNNING` concurrent turns, default 8, so a fan-out cannot exhaust the model budget; beyond
  it sessions wait in log order. `// ponytail: one fixed cap; per-tier caps if top sessions starve.`

## 3. Engine and tools

`~/dev/jevharn/src` is copied to `harness/bench/src/engine/` minus `pi.ts`, `cli.ts`, `ui.ts` and
`approve.ts` (terminal-only). The pi packages leave `package.json`. The LLM seam stays
`makeAiSdkLlm`; the model comes from the session row.

- `tools.ts` is the one file whose meaning changes. Its local backend (`execFile`, `spawn`,
  `readFile`, `inside(cwd)`) becomes an HTTP client to the session's workspace:
  `POST http://{podIP}:7788/tools/{name}` with the tool server's own names. Path confinement is
  the pod's `paths::confine`; the bench keeps no copy. The pod address comes from
  `/v1/workspaces/{id}/tools` as today; the row's existing `target` field carries the workspace.
- `patch` and the graft tools come free once the client lists `GET /tools`.
- `delegate`, `push`, spawn and delete are bench-side tools that go through the operation
  executor with its approvals and durability untouched.
- The engine's `.jevharn/project.md` notes and `procs/` logs live in the workspace, not on the
  bench: a clone must carry them and the bench holds no source.

Skipped: streaming tool output over the pod's two WebSocket routes, add when a long `exec` needs a
live tail in the UI.

## 4. Subagent lifecycle

Main's engine calls `delegate` with no target. Bench side, through the operation executor, in
order, each step a log row so a crash resumes at the right step:

1. **Spawn**: clone main's workspace (`POST /v1/workspaces/{main}/clone`). New session row: tier
   sub, parent main, workspace = clone id, state open. Child `user` row with the instruction, then
   the parent's `delegate` row.
2. The child's turn runs on the clone over 7788. It commits on a branch `sub/{seq}` in the clone.
3. On the child's `turn.end`, push from the clone pod into main's pod: `exec` in the clone of
   `git push ssh://kl@{main podIP}/home/kl/{ws} sub/{seq}:{main's working branch}`. Two platform
   changes, both small:
   - every workspace's prelude sets `git config --global receive.denyCurrentBranch updateInstead`,
     so it holds for every repo; safe because main never has uncommitted edits;
   - `authorized_keys_for` appends the owner's platform public key, so main's sshd admits the
     clone. Host keys are `StrictHostKeyChecking=accept-new`, as the seed clone already uses.
   A push rejected as non-fast-forward means main moved: the child gets one `user` row "rebase
   onto main and push again", one retry, then it reports failure.
4. Parent `user` row `from: child` with the final answer and the pushed commit id.
5. Delete the clone workspace; child row `state: closed`, kept read-only for review. The delete
   runs only after step 4's row is written, so a crash never loses the answer.

A child's `ask_user` goes to the parent as a `user` row, never to the person. The parent answers
with `delegate` to the same child seq, which reuses the open child instead of cloning.

## 5. Top and main sessions, HTTP surface, errors, tests

- **Top** is created once per bench, has no workspace, and `delegate(main, instruction)` names a
  main by its workspace name; an unknown name is a tool error listing the mains. Mains are created
  by the person through the existing create flow (`kind: workspace`, tier main), never by top.
- **HTTP**: `server.ts` routes keep their shape; rows carry `tier`, `parent`, `state`. One new
  read, `GET /sessions/{id}/children`. Delegation needs no route. The WebSocket message stream
  carries every row kind so the UI shows a child's progress under its parent.
- **Errors**: a model call failure ends the turn with `turn.end {error}` and leaves the unread rows
  unread, so the next inbound message retries with them. An unreachable tool server (workspace
  stopped, node dead) is a tool error the engine reports in its answer; nothing is retried blindly.
  A clone or delete refused by `/v1` (quota 409, interrupted source) is the child's answer to the
  parent verbatim.
- **Restart**: the section 1 boot scan, then the scheduler. A sub that was mid-turn is
  `interrupted`; its clone still exists, and the parent is told once via a `user` row so it
  decides: re-delegate to the same child (resumes on the existing clone) or close it.
- **Tests** (`bun test`, no cluster): a fake tool server as one in-process HTTP handler and a
  scripted `Llm`. Cases: delegate writes both rows in order; a crash between the two answer rows
  is repaired on boot; an interrupted turn is marked and not resumed; the cap is honoured; a
  non-fast-forward push retries once; a read-only main refuses `write`. Fleet proof: one hourly
  probe `bench.delegate` (top to main to sub, the push lands on main's branch, the clone is gone,
  the child is closed).

## Out of scope

Cross-node clones, cached `check_plain` answers, streaming tool output, log compaction, per-tier
run caps, PTY.
