# Bench sessions: platform tools only, workspaces through their tool servers

Owner rulings (2026-09-17 05:00–05:10 IST), from one bench transcript: asked "add nats to the
env", the model had no tool for it, so it read `/opt/harness/pi/kloudlite.ts`, cat'd the tool
token into ad-hoc node scripts against `/v1`, then created a second `devstack` and deleted the
first. Rulings: "you need to update all tools in the pi session"; "I don't like the fact it is
going and reading kloudlite.ts"; "it should be deprived of fs tools"; "actually it should be
deprived of exec tool too. it should use workspace ide tools directly".

## Design

1. **A bench session's only hands are its own workspace's** (owner, 06:10 IST: "bench session
   should not have access to any ide-tools of other workspaces other than itself"). It loads
   `workspace-tools.ts` pointed at its OWN workspace container's tool server
   (`KL_TOOLS_ADDRESS=127.0.0.1:7788`, the same server its shell splices to), so read/write/edit/
   bash/grep/find/ls act in `/home/kl/workspaces/bench` — never in the bench container, never in
   another workspace. pi's builtins stay off (`--no-builtin-tools`), because they would run in the
   bench container.
   **Nothing else runs in the bench container.** pi is spawned with `--no-builtin-tools`;
   `background.ts` and `process.ts` (both bench-pod `bash`) are not loaded for a bench session.
   The `btw` fork runs with `--no-tools`. Workspace sessions are unchanged (`--tools` allow-list
   on their own workspace).
2. **Work for a workspace is queued into that workspace's session, and done there** (owner,
   05:25 IST: "it should send message to workspace in the queue and it need to be processed
   there"). No `kl_ws_*` direct tool-server tools. One tool, `kl_workspace_ask { workspace,
   request }`: the extension calls the bench's own server (`POST /workspaces/{id}/ask
   {text, from}` on `127.0.0.1:7789`; `from` is the asking session's id, handed to the child at
   spawn as `KL_SESSION`). The bench opens the workspace thread (`openWorkspace`), records an
   exchange (`dir: "out"`, state `queued`), sends the text as a prompt to that child (`followUp`
   when it is mid-turn), transitions the exchange to `running` on its `agent_start` and `done` /
   `failed` on `agent_end`, and delivers that turn's final assistant text back into the asking
   session as a follow-up prompt prefixed `[from workspace <name>]`. Asks are never refused for being busy (owner, 06:20 IST: "any message sent to workspace
   session should be queued and workspace session will choose its own priorities and send back
   message to the bench session that sent the message"): every ask is queued into the workspace
   session as a message tagged `[ask <exchange> from <asking session name>]`, pi holds the queue,
   the workspace session works through them in its own order, and each finished turn's answer
   goes back to the session that sent the ask it answered (FIFO per workspace; an answer naming
   `[reply <exchange>]` in its text is matched by that id instead). The tool answers at once:
   "queued in <name>'s session; its reply arrives here". The workspace session (its own pi, its
   own tools) is what does the work, visible in that workspace's tab.
2a. **Its own workspace is the default target.** The bench IS a workspace (`KL_WORKSPACE_ID`).
   "Install X", "add a package", "switch env" with no workspace named act on the bench itself:
   `kl_pkg_list | kl_pkg_add | kl_pkg_rm | kl_pkg_update` (PATCH / POST on
   `/v1/workspaces/{own id}`) and `kl_env_current | kl_env_switch | kl_env_clear`
   (`/v1/me/environments/{KL_TEAM}`). A workspace session gets the same `kl_pkg_*` tools acting
   on its own workspace (`KL_TOOLS_WORKSPACE`). **Every workspace session, the bench included,
   manages its space's environment** (owner, 06:05 IST): `kl_env_current | kl_env_switch |
   kl_env_clear`, `kl_environments | kl_environment`, `kl_environment_service_add | _rm`, and
   `kl_intercept` (any service of the environment to any workspace of the space) are registered
   in workspace mode too, and named in `WORKSPACE_TOOLS`. `kl_workspace_packages` (by arbitrary id) is
   removed from the bench: another workspace is only ever asked. The system prompt says so.
3. **The catalogue covers `/v1`.** New tools (all through `kloudlite.ts`'s `reg`, named in
   `catalog.ts` with effect): `kl_environment_services` (PATCH `/v1/environments/{id}` `{services}`,
   effect write), `kl_workspace_packages_update` (POST `/v1/workspaces/{id}/packages/update`),
   `kl_workspace_restore` (POST `/v1/workspaces/restore`), `kl_environment_restore` (POST
   `/v1/environments/restore`), `kl_environment_restore_in_place`, `kl_volume_history` (GET
   `/v1/volumes/{name}/history`), `kl_volume_delete` (DELETE, effect delete), `kl_requests` /
   `kl_request_create` (`/v1/requests`). `kl_workspace_ask` is the exchange the regex already matches.
4. **`PATCH /v1/environments/{id}` `{services}`** is new on the api: `check_services`,
   `guard_alloc` for the ADDED service count only (removals free), merge-patch `spec.services`;
   refused 409 while an intercept names a service being removed. The controller prunes: a
   StatefulSet or ClusterIP Service in the env namespace whose name is not in `spec.services`
   is deleted on the next reconcile (owned by the Environment, labelled as today). Mount folders
   are never deleted — bytes stay on the volume until the volume goes. `env_doc` unchanged.
5. **The model is told where it stands.** `kloudlite.ts` adds a system-prompt line via pi's
   extension hook: it runs on a bench with no filesystem or shell of its own; workspaces are
   asked through `kl_workspace_ask`, which queues into that workspace's OWN session (created if it has none — owner, 05:28 IST: "there should be separate session created for workspaces and there the message will be queued"); its own packages and env through `kl_pkg_*`/`kl_env_*`; the platform only through `kl_*`; it never probes the platform
   another way. **It does not know it is pi** (owner, 05:15 IST): the extension REPLACES pi's
   default system prompt rather than appending — no "pi", no coding-agent boilerplate, no paths
   under `/opt/harness`; it reads and acts as "the Kloudlite harness" (owner, 05:17 IST) — the person's bench on the platform — and its tools are the whole world it sees.
6. **Probe.** Hourly `env.services.patched`: add a service to the run's environment via the
   PATCH, both ready; remove it, its StatefulSet gone within the stage budget. `bench.tools.no_fs`
   (hourly): the bench session's tool list has no `bash`/`read`/`write` and has `kl_workspace_ask`.

## Out of scope
Renaming a service in place (remove + add). Per-tool approval in the desktop. Bench `bash` for
power users (owner ruled against).

## 7. Background processes are tools again (owner, 06:45 IST: "if not we should include")

Every session's `bash` takes `background: true` and there is a `process` tool (`start | list |
logs | stop | write`), both mapped in `workspace-tools.ts` onto the session's OWN workspace tool
server: `exec {detach:true}`, `process_list`, `process_output`, `process_kill`, `process_write`.
A background command therefore runs inside the workspace (the bench's own container for a bench
session), never in the bench container, and survives the model's turn. `harness-bench` mirrors the
tool server's process list into its `/procs` ledger (session, id, cmd, state) on each
process-tool result so the desktop's Processes panel and `/proc-stop` work again; `/proc-stop`
calls `process_kill` on the owning session's tool server. `WORKSPACE_TOOLS` names `process`.

## 8. Structured results rendered, caveman prose (owner, 2026-09-17 13:10 IST: "show proper
rendered view using structured outputs … use caveman skill to reduce the amount of content")

- **The tool result is the view.** Every `kl_*` tool already answers JSON; the desktop renders it
  as a card instead of a raw JSON block: workspace doc (name, id, state, node, packages + status,
  env), environment doc (services table: name, image, ports, ready; intercepts), quota (used/limit
  bars), volume history (rows), ask (exchange chip: queued → running → done, the answer inline),
  process rows, package list, `kl_capabilities` (grouped list). Unknown shapes fall back to the
  JSON block. Renderers live in `harness/src/renderer/components/results/`, one file per doc kind,
  chosen by tool name in the action row.
- **The model does not repeat what a card shows.** Identity: "The person sees every tool result
  rendered; never repeat its fields. Your text is one line: what happened, or what you need."
- **Caveman prose.** The identity carries the caveman compression rules (vendored from
  `harness/pi/caveman.md`, from github.com/JuliusBrussee/caveman skills/caveman/SKILL.md, full
  level): drop articles and filler, fragments fine, short synonyms, technical terms and error
  strings exact, never drop negations, no invented abbreviations, no arrows, auto-clarity for
  warnings and irreversible actions. Applies to the model's chat text only — code, files, commit
  messages and anything written into a workspace stay normal prose.

## 9. State changes are asked first (owner, 2026-09-17 13:30 IST: "when ever we are changing the
state of the system … properly prompt the user about what's happening as question and answer")

- Every `kl_*` tool with effect write or destroy — except `kl_workspace_ask` (a message) and the
  own-machine `kl_pkg_*` — is a PROPOSAL, not a call. `makeReg`'s execute publishes
  `harness:proposal` `{id, tool, args, summary}` (summary from a per-tool one-liner in the
  catalogue: "Create workspace svelte-backend in centralindia-k3s with nodejs, go") and then
  long-polls the bench (`GET /proposals/{id}/wait`, honouring the tool's AbortSignal, 10 min cap).
  The desktop renders the proposal as a question card in the transcript — title, the summary, the
  fields, Yes / No — and answers `POST /proposals/{id} {answer}`. Yes → the extension runs the
  call and the result renders as its card; No → the tool answers "declined by the person" and the
  model stops. The answer is recorded as a user row ("yes" / "no") so the transcript reads as a
  conversation. Unanswered at the cap → declined.
- The model's text around a proposal is one line saying why; it never re-lists the fields.
- Prose is rendered as prose: the transcript's assistant text uses the UI face, not monospace;
  inline code stays a chip; lists tight; no raw markdown symbols.
- Motion: every transition is transform/opacity only, 120–160 ms, `prefers-reduced-motion`
  honoured; streaming text must not reflow earlier rows (fixed row heights for action rows,
  `content-visibility` where cheap); drawer and tab switches never drop frames from layout thrash.

## 10. Code and containers are tools (owner, 2026-09-17 14:20 IST: "give access of code repos to
our sessions … also containers, building images and push images etc as tools")

Repos, through `/v1` (every session): `kl_repos {owner?}`, `kl_repo_create {owner?, name,
visibility?, description?}`, `kl_repo_branches {repo}` (new `GET /v1/repos/{o}/{n}/branches`),
`kl_pulls {repo, state?}` (new `GET /v1/repos/{o}/{n}/pulls`), `kl_pull {repo, number}`,
`kl_pull_create {repo, title, head, base, body?}` (new `POST /v1/repos/{o}/{n}/pulls`),
`kl_pull_merge {repo, number, method?}`, `kl_pull_close`, `kl_compare {repo, base, head}`,
`kl_commit {repo, branch, message, patch}` (existing `POST …/commits`). Code itself is worked on
in the session's OWN workspace with git over ssh: `kl_repo_clone {repo, dir?}` runs
`git clone ssh://git@{host}/{owner}/{name}.git` there (host from the workspace's own git config
the prelude wrote; the owner's key is already in the pod). Push/branch/commit locally = plain
`bash git …` in the own workspace.

Containers, in the session's OWN workspace through the `kl` CLI (builder + registry credential
already there): `kl_container_build {context, tag, dockerfile?}` → `kl container build -t <tag>
[-f <dockerfile>] <context>` as a background process (a build is long; answers the process id,
progress via `process logs`), `kl_container_push {from, to}` → `kl container push`, `kl_images
{owner?}` → new `kl container images` (registry `/v2/_catalog` + tags with the registry token)
wrapped as a tool. Every write here is a proposal per §9 except the local git operations and the
build itself (it writes only to the registry under the person's own name).

## 11. Behave like a user (owner, 2026-09-17 15:05 IST: "the tools are exposing lot of internal
functionality … it checked the quota before creating … it should behave like user. keep the
skills simple.")

- **Tool surface = what a person does.** Keep: workspaces (list, create, start, stop, snapshot,
  snapshots, restore, clone, delete, ask, progress), own packages (list, add, rm), environment
  (current, switch, clear, list, get, create, delete, service add/rm, intercept, snapshot,
  snapshots, restore), repos (list, create, branches, pulls, pull, pull create/merge/close,
  clone), containers (build, push, images), capabilities, own hands. **Remove** from the model:
  `kl_quota`, `kl_regions`, `kl_volumes`, `kl_volume_history`, `kl_volume_delete`, `kl_builder`,
  `kl_whoami`, `kl_requests`, `kl_request_create`, `kl_workspace_packages_update`,
  `kl_pkg_update`, `kl_compare`, `kl_commit`, `kl_environment_restore_in_place` (folded:
  `kl_environment_restore {id, snapshot}` restores in place; a new env from a snapshot is
  `kl_environment_create {from_snapshot}`), `kl_workspace_restore` likewise folded into
  `kl_workspace_create {from_snapshot}`. Snapshots are named "snapshot", never volume/history.
- **No parameters a person would not type.** `region` and `owner` disappear from create tools:
  the extension fills region from its own workspace doc and owner from the space (`KL_TEAM`).
  Ids stay accepted, names are accepted too and resolved by list.
- **No pre-checks.** Identity: "Do what was asked, directly. Do not check quota, regions,
  or current state first. If a call fails, say the error in one line and stop." Errors from the
  platform are surfaced verbatim (the 409 sentence is already written for people).
- **The prompt says only what the model must know.** Identity is cut to: who it is; its own
  machine is the default; other workspaces are asked; new component = new workspace; never
  behind the tools; never an unasked write; no pre-checks; brevity + caveman. Everything
  operational (proposals, rendering, tags) is mechanism, not prompt text, except the one line
  about `[ask]` tags a workspace session needs.

## 12. Agents, the way Claude Code does it (owner, 2026-09-17 15:20 IST: "the behaviour of the
bench sessions should be exactly similar to that of claude code … creates agents, and branch out")

Claude Code's shape, mapped onto the harness:
- **Agent** (fresh context, one task, own tools, runs in the background, reports back once) =
  `kl_agent {task, workspace?, name?}`. It opens an EPHEMERAL session (`openEphemeral`) in the named
  workspace (default: the caller's own machine), seeds it with the identity plus ONLY the task text,
  runs it to completion, and delivers its final answer back to the caller as `[from agent <name>]
  …`. Several may run at once; the caller continues meanwhile. An agent cannot spawn agents. Its
  transcript is an ephemeral tab in the desktop (exists today) and closes when the caller is done
  with it (`kl_agent_close`). Answer to the caller at dispatch: "agent <name> started" — one line.
- **Fork** (inherits context, read-only, one question) = the existing `btw`, surfaced to the model
  as nothing: it is the person's verb, not the model's.
- **Teammate** (persistent, its own memory) = `kl_workspace_ask` into a workspace's own session.
- **Plan / todo** = `kl_plan {items}` and `kl_plan_done {item}` feeding the inspector's GOAL and
  PLAN panels (MachineView/PlanTree already draw them); goal = the first message, as today.
- **Prompt guidance** (three lines, in the identity): "Independent work that does not need your
  context goes to an agent with a precise brief; keep its conclusion, not its transcript. Run
  agents in parallel when tasks are independent. Write the plan first when the work has more than
  two steps, and tick items as they land."
- **UI behaviour follows the same shape** (owner, 15:25 IST: "ui behaviour should also be
  similar. it should show loading, queuing.. etc"): while a turn runs, one status line under the
  transcript — a spinner glyph, a changing verb ("Thinking…", "Running bash…", "Waiting on agent
  svelte…"), elapsed seconds and tokens so far; prompts typed meanwhile appear beneath it as
  queued rows (›) in order and move into the transcript when taken; tool rows collapse to one line
  with a chevron, expanded on click; an agent shows as a row with its name, state and elapsed,
  its report folded under it; a question card blocks the status line until answered.

## 13. Twelve tools, the rest deferred (owner, 2026-09-17 15:40 IST: "43 tools is huge … plan
things so simple … add skill/toolsearch")

- **Always on (12):** read, write, edit, bash, grep, find, ls, process (own machine); `ask`
  (a workspace's session or a fresh agent: `{to: <workspace|"agent">, task, name?}` — replaces
  kl_workspace_ask + kl_agent); `plan`; `skill {name}`; `tool_search {query}`.
- **Deferred:** every `kl_*` platform tool stays registered but INACTIVE (`pi.setActiveTools`);
  `tool_search` matches the catalogue by name/summary, answers the matched tools' names and
  parameters, and activates them for the rest of the session. Nothing else changes about them
  (proposals, cards, waits).
- **Skills:** short markdown under `harness/skills/` — `workspaces`, `environments`,
  `snapshots`, `repos`, `images`, `agents` — each a screen of product words: what the thing is,
  the verbs, one example. `skill` returns the text; the identity lists their names in one line.
- Identity shrinks by the tool paragraphs: it names the skills and says "tool_search finds the
  tool for a platform verb".

## 14. Memory (owner, 2026-09-17 15:50 IST: "just like claude code will update the knowledge
this also should update the knowledge")

Claude Code's auto-memory, on the bench: a per-person memory directory in the bench workspace,
`{ws}/.bench/memory/` — `MEMORY.md` (one line per memory, the index) plus one file per memory
with frontmatter `name`, `description`, `type: user | feedback | project | reference` and a body
("**Why:** … **How to apply:** …" for feedback/project). Core tool `memory {save?: {name,
description, type, body}, forget?: name}` (13th always-on tool); `MEMORY.md` is appended to the
identity at every session start (bench and workspace sessions alike; it is the person's memory,
not the session's). Identity rule: "When the person corrects you, states a preference, or tells
you a fact about their setup you will need again, save a memory. Never save what a tool can
answer." A `[from …]` reply or an ask is never saved. Snapshotted with the bench like everything
under `.bench/`. Desktop: Settings › Memory lists the index with delete.
