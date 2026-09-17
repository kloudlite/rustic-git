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

## 15. Inbox triage and parallel work (owner, 2026-09-17 16:20 IST)

- **A fork orders the inbox.** Whenever a session's queue changes while it is mid-turn (a person's
  prompt, an ask, a reply), the bench spawns a FORK of that session (`--fork`, `--no-tools`,
  inherits the context, one question): "here is the queue, answer the order as JSON, most urgent
  first, with one reason each". The bench then re-queues pi's follow-ups in that order
  (`clear_queue` returns the texts; re-`follow_up` them) and shows the order and reasons in the
  queue panel. The main session is never interrupted; the fork's cost is one call per change,
  debounced 3 s.
- **A received message may be worked alone or in parallel.** `ask {to: "agent", isolated: true}`
  gives the agent its OWN ephemeral workspace: a clone of the caller's workspace
  (`kl_workspace_clone`, name `<ws>-eph-<hex>`, marked ephemeral) whose session is the agent's;
  several agents run in parallel on their own clones; the agent's report says what changed and
  where; the clone is deleted with `ask_close` (or when the caller's session ends) after the
  person, or the caller, has taken what it wants (git push from the clone, or a `later` plan
  item). Not isolated = the agent shares the caller's workspace, as today.

## 16. The centre pane looks like opencode, dressed in Zed (owner, 2026-09-17 17:10 IST)

Reference: opencode's TUI session view. Everything in the centre pane is monospace (this
supersedes §9's UI-face prose for the transcript; cards keep their structure but in mono).
- **Header**: `# <session title>` left; right: tokens used · context % · cost, muted.
- **Person's message**: a block with a 2 px left accent border and a slightly lighter background.
- **Assistant text**: plain, no bullet rail, no card; paragraphs separated by one blank line.
- **Tool calls**: one muted line each, glyph + verb + argument + result count:
  `∗ Grep "homepage|home.*button" (18 matches)`, `→ Read path/to/file.tsx`, `$ npm test (exit 0)`,
  `~ Asking questions…`, `⇢ ask svelte-frontend: …`, `◐ agent audit running 12s`. Click expands the
  result (the code block/card from §8/§13). Consecutive tool lines group with no gaps.
- **Turn footer**: `▣ <mode> · <model>` muted, one line, after each assistant turn.
- **Composer**: block with the same left accent border; below the input one status row:
  mode (accent), model, provider (muted). Footer bar: a dotted progress glyph while a turn runs,
  `esc interrupt`, and the key hints (`⌘J shell`, `⌘L prompt`, `⌘K commands`) right-aligned.
- **Theme**: Zed's default *One Dark* palette as tokens (background #282c33, surface #2f343e,
  text #dce0e5, muted #838994, border #464b57, accent blue #74ade8, green #a1c181, red #d07277,
  yellow #dec184, purple #b477cf) with a One Light counterpart for the light theme; fonts: Zed
  Plex Mono for everything in the pane, Zed Plex Sans for the shell chrome (both OFL, vendored as
  woff2 from zed-industries/zed `assets/fonts`). Line height 1.5, 13 px, 2-space glyph gutter.
- Inspector and sidebar keep their layout, take the palette and Zed Plex Sans.

### 16b. Observed in opencode 1.18.31 (tmux, 2026-09-17 17:40 IST) — the contract to replicate

- **Empty session**: wordmark centred; composer block with a left rail `┃`, placeholder
  `Ask anything… "Fix a TODO in the codebase"`, second line `Build · <model> <provider> · low`,
  bottom rule `▀▀▀`; under it right-aligned `tab agents  ctrl+p commands`; a `● Tip …` line;
  footer: `<cwd>:<branch>` left, `⊙ 3 MCP /status` and version right.
- **Person's message**: rail block `┃  text` (one blank rail line above and below).
- **Assistant text**: plain, indented to the rail's text column, no glyph.
- **Tool lines**: read/glob/grep are ONE muted line `→ Read index.ts`; **edit** is a rail block
  `← Edit index.ts` followed by the diff with line numbers and `+`/`-` rows inside the rail;
  **bash** is a rail block `$ ls -a && wc -l index.ts` with its output lines inside the rail;
  **question** is a rail block: the question, numbered options with a one-line description each,
  `3. Type your own answer`, hint `↑↓ select  enter submit  esc dismiss`; **subagent** is
  `✓ Explore Task — <title>` then `↳ 1 toolcall · 2.3s` then `ctrl+x down view subagents`, its
  report as plain text after; while running the ✓ is a spinner glyph.
- **Queued message** typed mid-turn shows as a rail block at the point where it was taken.
- **Turn footer** after every assistant turn: `▣  <mode> · <model> · 5.2s` (muted; elapsed).
- **Composer** while running: same block; **footer bar**: animated `⬝■■■■■■⬝` spinner + `esc
  interrupt` left; `70.0K (7%) · $0.01  ctrl+p commands` right. Idle: cwd left, hints right.
- **Sidebar (right)**: session title (auto-named from the first message), `Context` block
  (`69,738 tokens`, `7% used`, `$0.01 spent`), MCP list with connection dots, LSP, cwd:branch.
- **Keys**: `tab` cycles mode Build/Plan (shown in composer and turn footer); `ctrl+t` cycles the
  model variant (low/high); `ctrl+p` opens a command palette overlaid on the right: Search box,
  groups Suggested / Session, each row with its shortcut (`ctrl+x l` switch session, `ctrl+x n`
  new, `ctrl+x m` switch model, `ctrl+r` rename, `ctrl+x g` jump to message, fork, compact,
  undo, hide sidebar); `/` opens a slash menu listing commands with descriptions; `esc`
  interrupts.
Map: mode ↔ our bench/plan-only toggle (plan mode = read-only tools + plan tool); variants ↔ model
thinking level; MCP list ↔ our tool servers / connected workspaces; `ctrl+x down` ↔ agent tabs.

## 17. Claude Code's behaviour, adopted (owner, 2026-09-17 17:55 IST; read off this very
session's transcript: 486 agent dispatches, 217 background commands, 188 follow-up messages to
running agents, 8 monitors, questions to the person, skills and tool_search)

What Claude Code does, and what the harness does for it:
1. **Dispatch with a brief, receive a report.** An agent is started with a one-line description,
   a model tier, and a brief; it answers with a status (`DONE | DONE_WITH_CONCERNS | NEEDS_CONTEXT
   | BLOCKED`), commits/changes, a one-line test summary and concerns, and writes the long report
   to a FILE the parent reads only if needed. Harness: `ask {to:"agent"}` takes `brief` (text) and
   optional `model`; the agent's identity tells it the four statuses and that its final message
   is the report; the reply row shows status + one line, the full text folded.
2. **Agents are resumable.** A parent sends follow-ups to a named agent with its context intact
   ("fix round", "one more thing"). Harness: `ask {to: "<agent name>", task}` on a live agent
   resumes it; `[from agent]` replies route as before. An agent stays until `ask_close`.
3. **Background work notifies; the parent does not poll.** Long commands run detached; a
   completion notification arrives as a message (`[task <title> finished: exit N]`) with the
   output file path; monitors stream matching lines as messages. Harness: `bash {background:
   true}` and `process start` deliver a follow-up message on exit with the last 20 lines; a
   `watch {process, pattern}` action on `process` emits a message per matching line; all sent
   through the same follow-up path so the model handles them in turn order.
4. **A person's message mid-turn is surfaced as such.** The model sees "the person sent a new
   message while you were working" and addresses it in the same turn. Harness: a queued prompt
   is delivered as a steer with that prefix line (the fork-ordered queue still applies to
   follow-ups; a person's own message is a steer, first).
5. **Only the model sees tool output.** A reminder after long output: "only you see this; relay
   what the person needs". Harness: tool results over 40 lines carry that trailer line.
6. **Questions to the person are a tool** (`AskUserQuestion`: header, question, 2–4 options with
   descriptions, multi-select, free text). Harness: core tool `question {header, question,
   options[{label, description}], multi?}` rendered as the §16b card; the answer returns as the
   tool result and as the person's row.
7. **Plan mode / read-only exploration** before changing things; **skills invoked before work**;
   **tool_search** for deferred tools; **memory** saved when corrected — all already in §12–§14
   and §16b (`/mode plan`).
8. **Context management.** When the conversation grows long it is summarised and continues.
   Harness: at 80 % of the model's window the bench sends `/compact`-equivalent (pi `compact`)
   with a summary prompt that keeps the plan, open asks and memory index; the transcript shows a
   `⟲ compacted` row.
9. **Stop and cancel.** A running background task can be stopped (`TaskStop`); esc interrupts the
   turn; a stopped turn's tools are cancelled. Harness: `process stop`, `esc` → pi `abort`, and
   `ask_close` on a running agent aborts it first.
10. **Reporting to the person**: lead with the result; failures with cause and fix; never claim a
    fleet fact unverified. Already in the identity (caveman + brevity); add: "A thing you changed
    but could not verify is 'changed, unverified'".

## 18. Replies to the bench are to the point (owner, 2026-09-17 18:15 IST)

The bench session is a planner and orchestrator; it does not hold the code. A workspace session
owns its code and answers the bench like a teammate at standup, never with a code dump:
- A reply to an ask is ≤ 8 lines: status word first (`done` / `partial` / `blocked`), what changed
  (files by NAME, not content), how it was verified (one line), what is left or what it needs.
  No code blocks, no diffs, no command output, no step-by-step narrative. Details stay in the
  workspace session's own transcript, where the person can read them in that tab.
- Mechanism, not only prompt: the bench truncates a `[reply]` to 12 lines / 1,200 chars before
  delivering it (the full text stays in the workspace transcript and behind the reply row's
  fold) and strips fenced code blocks from what the bench session receives.
- The workspace identity says so verbatim; the bench identity says: "You do not read code. Ask
  the workspace; its reply tells you what changed and where."

## 19. Two kinds of message: work and information (owner, 2026-09-17 15:15 IST)

`ask` gains `kind: "work" | "info"` (default `work`; the model must choose, the tool description
says how: "info = a question about the workspace's code or state that changes nothing").
- **work**: as today — queued into the workspace's own session, FIFO, fork-ordered, done there.
- **info**: never touches the workspace session's queue or turn. The bench FORKS the workspace's
  running session (`--fork` of its transcript, read-only tools only — read/grep/find/ls — plus
  its context, KL_FORK identity) and asks the question there; the fork answers once, in the §18
  short shape, and is discarded. Many info asks run in parallel; the workspace session is never
  interrupted. If the workspace has no session yet, the fork is of an empty session in that
  workspace (tools only, no history). The reply row says `[info from <name>]`. Plan: an info ask
  is NOT a plan item.
- Identity line (bench): "Ask a workspace for information with kind: info — it answers from a
  read-only copy without stopping its work. Ask for work with kind: work."

## 20. Parallel tool calls (owner, 2026-09-17 15:50 IST)

Claude Code runs independent tool calls of one turn at once and shows them as one group row:
`● Running 6 shell commands · 5m 10s…` with each command as a sub-row (`└ $ …  (5m 9s)`) that
updates live. The harness does the same: when a turn issues several tool calls, the tool server
runs them concurrently (each `bash`/`exec` is its own job already; the extension must not await
them serially — issue all, await all), and the desktop folds consecutive tool rows of one turn
into a group row with a count, the running verb, the group's elapsed time, and the sub-rows;
the group collapses to its one line when done. Identity: "Independent commands go in one turn,
together; they run at the same time."

## 21. Observed in Claude Code v2.1.274 (tmux, 2026-09-17 15:30 IST) — behaviour AND render to clone

The person's message: `❯ text`; a message queued mid-turn is shown indented under it as it is
taken (`  also tell me the git branch`).
While working, ONE status line that changes: `Reading 1 file, listing 1 directory, running 1
shell command…` (a live summary of the parallel calls, with `⎿ index.ts` sub-line), then the
spinner line `✳ Embellishing… (3s · ↓ 123 tokens · thought for 1s)` — a rotating whimsical verb,
elapsed, tokens, thinking time. When done the summary collapses to past tense: `Read 1 file,
listed 1 directory, ran 1 shell command`.
Assistant rows start with `⏺`; a tool row is `⏺ Update(index.ts)` / `⏺ Explore(Count functions in
index.ts)` with a `⎿` result line (`Added 1 line` + the diff with line numbers and `+`;
`Backgrounded agent (↓ to manage · ctrl+o to expand)`); long results are folded to a line with
`… +N lines (ctrl+o to expand)`. Turn footer: `✻ Crunched for 9s · done 3:33 PM` (+ `· 1 shell
still running` while a background task lives). Notifications land as their own `⏺` rows when they
arrive: `⏺ Agent "Count functions in index.ts" finished · 5s`, `⏺ Background command "Background
sleep task" completed (exit code 0)`, `⏺ User answered Claude's questions:` `⎿ · Next file name:
alpha or beta? → alpha`. The question card: `☐ File name` header, the question, numbered options
with descriptions, `3. Type something.`, `4. Chat about this`, `Enter to select · ↑/↓ to navigate
· Esc to cancel`. Composer `❯` between two full-width rules; footer `⏵⏵ accept edits on (shift+tab
to cycle) · esc to interrupt · ← for agents`; a right-aligned `○ low · /effort` chip; the todo
list appears as a `⏺ Todo:` row.
The harness clones: the live parallel summary line + past-tense collapse, the spinner line with
verb/elapsed/tokens/thinking, `⏺`/`⎿` rows for tools and notifications (agent finished,
background finished, question answered), `… +N lines` folds, the turn footer with "N still
running", the `☐ header` question card with "Type something" and "Chat about this" (the latter
sends the question back to the model as a normal prompt), and `shift+tab` cycling the mode
(build → plan → accept-edits). Keep opencode's rails, header and palette (§16b) where the two
differ in chrome; take Claude Code's row grammar and status behaviour.

## 22. Agents are subagents with their own workspace (owner, 2026-09-17 16:35 IST)

- **Default = own workspace.** An agent runs in its own ephemeral clone of the caller's workspace
  (complete code, same packages) — `isolated` is the default, `shared: true` is the opt-in for a
  read-only or tiny task in the caller's own workspace.
- **Finish, push, discard.** The agent's identity: "You have your own copy of the workspace. Do
  the task there. When done, commit on a branch named after you and push it (or open a pull
  request through the tools), then report with the branch or pull. Your copy is deleted after
  your report." The bench deletes the clone when the agent reports `DONE`/`DONE_WITH_CONCERNS`
  (after the report is delivered); `BLOCKED`/`NEEDS_CONTEXT` keep the clone until the caller
  answers or closes it; `ask_close` deletes it any time.
- **Direct line, no queue.** Caller ↔ agent messages are immediate (steer when mid-turn, prompt
  otherwise); no FIFO, no fork triage; exchanges are recorded only for the panel.
- The caller's report row shows the branch/pull the agent left behind so the person can take it.

## 23. Port opencode's session renderer, do not imitate it (owner, 2026-09-17 17:50 IST: "go
through code of opencode and try to replicate it instead of just trying to replicate from ui
screenshot")

opencode is MIT (`/Volumes/kdisk/rustic-git-wt/opencode-ref/LICENSE`). `@opencode-ai/session-ui`
is Solid, like the harness. The centre pane becomes a PORT of that package, not a re-drawing:
- `harness/src/renderer/opencode/` holds the vendored components (message parts, `BasicTool`,
  context group, text/reasoning/compaction, diff via `@pierre/diffs`, prompt/composer, footer,
  progress indicator, shimmer, spinner, question, task), their CSS and the theme JSON loader, with
  `LICENSE-opencode` beside them and a `VENDORED.md` naming the upstream commit (`5a83358`) and
  every file taken; edits to vendored files are minimal and marked `// harness:`.
- One adapter, `opencode/adapter.ts`: our live store (pi events, tool rows, exchanges, proposals,
  plan, procs, usage) → opencode's `Message`/`Part` shapes from `@opencode-ai/sdk` types (vendor
  the type definitions only). Our tools map to their tool kinds (the `opencode-map.ts` table).
  Questions/proposals → their `question` part + permission dock; agents → `task`; plan →
  `todowrite`; asks → `task` with `agent` = workspace name; harness notes → `text` with a system
  flag as they render it.
- Dependencies added as real dependencies where opencode has them: `@kobalte/core`, `motion`,
  `@pierre/diffs`, `shiki`/`@shikijs/stream`, `dompurify`, `morphdom`, `remend`, `luxon`,
  `fuzzysort`, `solid-list`, `strip-ansi`, the `@solid-primitives/*` used. `marked` we have.
- Theme: their `one-dark.json` + `one-light.json` loaded by their theme code; IBM Plex Mono/Sans
  set through their font variables. The terminal, sidebar and inspector keep our components.
- Result: pixel-parity by construction. Our previous hand-drawn pane components are deleted once
  the port renders every part; the render-contract document stays as the map.
