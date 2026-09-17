# Session boundaries: models, sidecar shells, hands-free sessions, subagent trees

Owner's brief (2026-09-17, 20:30–21:30 IST): "this is all design to set boundaries for
sessions". Four features, decided one by one in conversation; every decision below is the owner's
unless marked *proposed*. This document replaces the parts of
`2026-09-17-bench-tools-no-fs-design.md` and `2026-09-17-terminals-tmux-and-tabs-design.md` that it
contradicts (named in §8) and leaves the rest of both in force.

## 0. The one idea

A **session** is a model with a queue and a plan. It has **no hands of its own**: it cannot read,
write or run anything where it lives. Everything it does to the world goes through a **tool
interface** that names *which* tree of *which* workspace it is acting on, and every tool call is
checked against that tree before it runs. The person, by contrast, gets a **shell** that is only a
shell: a throwaway terminal in the person's home, with no route into the code and nothing the model
can drive.

Three boundaries fall out of that, and every section below is one of them made concrete:

| boundary | who is kept from what | mechanism |
|---|---|---|
| **session ↔ its own container** | the model from the sessions container's filesystem, network and processes | pi runs with no built-in tools; the harness registers only `kl_*`, ide and messaging tools; the container has no shell binary on the model's path and no token file the model can read |
| **session ↔ other trees** | a session from any tree but its own | every ide call carries `tree`; the tool server confines the path to that tree's root; the main tree's root excludes `.agents/` |
| **person's shell ↔ the platform** | the person's terminal from the tool server, token, harness files and code | the shell is a separate container with only the home mounted, running `ttyd`; nothing else listens there |

Nothing in this design gives a model a new capability. Each section removes one or moves one behind
a checked interface.

## 1. Model, thinking level and effort

### 1.1 Decisions

- Every session **remembers** its model, thinking level and effort, and uses them on every turn.
  These are session fields, not per-turn overrides.
- A pick made by the person **anywhere** becomes the **general default** for new sessions and for
  agents — unless the creating workspace session names a model explicitly when it dispatches.
- **Variant means effort.** The effort control is shown only for a model that has one; the picker
  never offers a knob the model cannot take.
- `Ctrl+T` cycles the thinking level, as in opencode.
- `/model` opens an opencode-style dialog: providers, then models, searchable. Every provider pi
  supports is **listed**; only DeepSeek is **wired** now, the others are placeholders. Provider
  credential flows (env-based, OAuth) come later.
- No phases. A session has no spec/plan/implement mode; the picker is the whole surface.

### 1.2 Shape

pi already carries the mechanism — `get_available_models`, `set_model`, `set_thinking_level`
(off, minimal, low, medium, high, xhigh) over RPC, keys in `~/.pi/agent/auth.json` written from
the desktop Settings. Effort is a pi model parameter where the provider has one (Anthropic
`effort`, OpenAI `reasoning_effort` is folded into the thinking level and needs no second knob).

The session record (`SessionRow` in `harness-bench`) gains three fields, all optional:

```
model?:    "provider/model-id"        // already present, becomes authoritative
thinking?: "off"|"minimal"|"low"|"medium"|"high"|"xhigh"
effort?:   "low"|"medium"|"high"|"max"  // only when the model takes one
```

and the bench keeps one **default triple** in its state (`{bench}/.bench/defaults.json`). The
bench applies the triple to a pi child on `session_start` (after `setActiveTools`, the same place
the identity is set), and again on every change the person makes, so a restarted bench comes back
on the same model without the person noticing.

Resolution order when a session is created:

1. an explicit model in the create call (a workspace session dispatching an agent may name one);
2. the general default (the last pick the person made anywhere);
3. pi's own default for the configured provider.

A pick in an existing session writes both the session's fields **and** the general default; a
dispatch that names a model writes only that session's fields. This is the whole rule for
"general default unless asked for".

### 1.2b Effort is stored, not yet applied (owner, 22:40 IST)

pi 0.85.1 has no effort parameter: `set_model` takes provider and model id only, and effort is
folded into its per-model thinking levels. The effort value is therefore **persisted** (session
field, default triple, footer) and **not applied** to pi. How it takes effect is a later decision;
nothing else changes here.

### 1.3 Surface

- **Status line** (composer footer, opencode row grammar): `deepseek/deepseek-reasoner · thinking
  high · effort max`. Segments that do not apply are absent, never shown as `—`.
- **`/model`**: dialog rendered in place of the composer like the permission prompt: ONE list in
  opencode's `/models` shape — a dim group header per CONFIGURED provider (owner, 00:20 IST 18 Sep:
  "show only configured"; the unwired providers appear only in Settings), its models indented
  beneath, a filter box that narrows across providers, the cursor row a full-width bar with the `❯`
  glyph, the picked model marked `●`. No tags (no `thinking` — owner). Enter picks, Esc leaves it.
- **`Ctrl+T`**: cycles thinking through the levels the model supports; the footer segment changes
  in place.
- **Effort**: a third row in the `/model` dialog, present only when the model takes one; `Ctrl+E`
  cycles it *(proposed key; opencode has no binding for this)*.
- Workspace sessions and agent sessions show the same footer; their model came from the rule above
  and can be changed the same way.

### 1.4 Probes and tests

- Bench test: creating a session with no model gets the default triple; a pick in session A
  changes the default and the next created session B has it; a dispatch naming a model does not move
  the default.
- Bench test: a restarted bench re-applies each session's triple on `session_start`.
- Renderer test: footer segments present/absent by model capability.


### 1.5 What enters the transcript and what enters the session (owner, 00:50–01:05 IST 18 Sep)

- The transcript shows: the person's prompts, the model's text and reasoning, tool rows, question
  and proposal cards, and one quiet DIVIDER line per state change that matters when reading later
  — `Model changed to <display name>`, `Thinking <level>`, `Effort <level>`, `Session compacted`,
  `Interrupted`. Nothing else: no slash-command echo, no harness notes, no plan-change lines
  (the PLAN panel is the surface), no error lines (errors are a footer status).
- **pi's session file and the model's context receive none of it.** Dividers are renderer state
  from bench events; `/proc-stop` and `/cancel` are bench HTTP calls, never prompts.
- When the model genuinely needs an outcome of the person's action (a process it started was
  stopped, a task cancelled), the bench delivers a structured `custom` message
  (`harness:event`, `{kind, id, title, by:"person"}`) on the model's next turn, drawn as the compact
  result card tool rows use — never a slash string, never a user-role message.


### 1.6 Renderer state has four owners (owner, 02:50 IST 18 Sep: "state hierarchy is not properly managed")

| level | owns | dropped when |
|---|---|---|
| app | connection, team, palette, settings, model catalogue | the window closes |
| workspace | files cache, watch stream, processes, background tasks, terminals | the workspace is deleted |
| session | transcript, plan, queue, proposals and questions, model triple | the session is archived |
| tab (a view of a session) | open file, scroll position, composer draft, open dialog, folds | the tab closes |

No state lives above its owner. A component reads from the level it belongs to; a global signal
for a tab's or session's concern is a defect (tonight's: open file surviving its tab, dialog and
footer read across sessions, exchanges cached under the wrong key).

## 2. The shell is a sidecar

### 2.1 Decisions

- Every bench pod and every workspace pod has a **shell container** beside its main one.
- The shell is a **bare image with `kl`** and the workspace's Nix profile binaries; on a workspace
  pod the profile is **shared** between the two containers (same `nix` store mount).
- It mounts the **home only**. It opens in the home. It never mounts the workspace directory, the
  tool token, `/opt/harness`, or anything of the main container's. "We are not providing access to
  the code directly via shell."
- It **persists nothing**, holds no intercepts, attaches to no environment, and its processes are
  visible nowhere. It is for the person's convenience and testing.
- The terminal transport is **`ttyd`**. No PTY route in the tool server, no session table, no
  replay. A dropped connection is a new shell.
- The sessions container **cannot exec into it**. Nothing in the pod can: there is no shared
  process namespace, no `kubectl exec` route exposed, no tool.

### 2.2 Pod layout

Bench pod (a Workspace with `spec.bench`):

| container | image | mounts | listens | runs as |
|---|---|---|---|---|
| `sessions` | bench image (`harness-bench` + locked pi) | `{ws}` (the bench's own volume, for `.bench/` state) | 7789 bench API (gateway hole, bench token) | uid 1000 |
| `shell` | shell image (`kl`, coreutils, the profile) | `home`, `homecache`, profile | `TTYD_PORT` 7790 | uid 1000 |

Workspace pod:

| container | image | mounts | listens | runs as |
|---|---|---|---|---|
| `workspace` | workspace image | as today: `{ws}`, home, homecache, live caches, keys, profile | 22 sshd, 7788 `kl ide serve` | uid 1000 |
| `shell` | shell image | `home`, `homecache`, profile | 7790 `ttyd` | uid 1000 |

The **sessions container's mount is a state store, not hands**: the model in it has no tool that
reads a path, and the harness reads `.bench/` for itself. That is §3.

### 2.3 ttyd

`ttyd -p 7790 -i 0.0.0.0 -W -t disableLeaveAlert=true -t fontFamily='IBM Plex Mono' zsh -l` from
the shell container's prelude, `cwd` the home. `-W` makes it writable; without a flag ttyd is
read-only, a trap worth a comment in the pod spec. Auth is the fence, not ttyd's basic auth: the
`allow-bench-tools` NetworkPolicy already admits the person's bench pod and the ssh tunnel to
`IDE_PORT`; it gains 7790 on both pod kinds, and nothing else can dial it. The desktop reaches it
through the bench tunnel exactly as it reaches 7788 (`/pty?scope=` becomes a splice to
`{pod}:7790`), and renders it with the xterm.js it already ships speaking ttyd's protocol
(one-byte opcode prefix: `0` input, `1` resize JSON; server `0` output, `1` title, `2` preferences).
The web page ttyd serves is never loaded; only its WebSocket is used.

Terminal tabs stay per session tab as today, but a tab is a live socket and nothing more: the
"Tabs are the sessions" reconcile (§2b of the terminals design) and the tool-server persistence
(§7 there) are **retired**.

### 2.4 Shell image

`deploy/shell-image/Dockerfile`: debian-slim, `kl`, `ttyd` (from the Nix profile so it tracks the
pin, or vendored if the profile is not ready before the shell must start — the prelude waits on the
profile symlink either way), the same `gitignore-global`, the same `zshrc`/starship as the
workspace image so the shell feels like the workspace's. No sshd (ssh lands in the workspace
container as today), no `kl ide`, no token, no harness. The image is built by `image.yml` beside
the others and pinned with them.

### 2.5 Probes

- `shell.up`: through the tunnel, open a ttyd socket to the workspace's and the bench's shell,
  run `pwd` → the home; `ls /home/kl/workspaces` on a workspace pod → refused/absent (no mount).
- `shell.fenced`: from a probe pod outside the fence, dial 7790 → refused.
- `shell.no_tools`: `curl 127.0.0.1:7788` from inside the shell → connection refused (the ports are
  in the pod's shared network namespace, so this **would** connect; the tool server therefore
  **requires the bench token on every request** — it already does — and the shell has no token.
  The probe asserts a 401, not a refused connection. Stated so nobody "fixes" it later).

## 3. Sessions have no hands

### 3.1 Decisions

- The sessions container hosts **every** session: bench sessions, workspace sessions, agent
  sessions. All pi children of one `harness-bench`.
- **No session can read, write or exec in the sessions container.** Not the bench session's own
  workspace directory, not the home, not `/opt/harness`. "Any file read or edit will have to happen
  through the tool interface."
- The bench session installs **no packages** anywhere; there is no "on the bench" target any more.
  A package request always names a workspace and becomes a proposal on that workspace's spec.
- A workspace session and an agent session reach **only their own tree** in their own workspace
  pod, over the network, through `kl ide` tools.

### 3.2 What changes in the bench

Today's bench session already runs pi with `--no-builtin-tools`; workspace sessions have ide tools
bound to their workspace. This section makes it total and removes the exceptions:

1. **`ownTools` goes.** The bench session's ide tools on `127.0.0.1:7788` are removed; there is no
   tool server on the bench pod to reach (the bench pod has no `workspace` container). `kl_pkg_*`
   with no workspace named is refused: "name the workspace".
2. **No shell on the model's path.** The bench image keeps `/bin/sh` for the entrypoint only; pi's
   `bash` tool is not registered in any mode, and the `harness:shell-gate` allow-list from the
   tools design is deleted with it (nothing left to gate).
3. **The token is not the model's.** `BENCH_TOOL_PATH` stays a read-only secret mount that
   `harness-bench` (uid 1000) reads; no init container and no `0400` root copy (decided at
   implementation, 18 Sep 00:40 IST): with no filesystem tool registered in the sessions container
   the boundary is the tool set, and a root-owned copy would need new machinery for no extra
   boundary. Every outbound call the harness makes carries the token; no tool exposes it.
4. **Identity says so.** One line in both identities: "You have no filesystem or shell where you
   run. Every read, edit and command is a tool call that names a workspace and a tree." The
   `skills/workspaces.md` "Packages" section drops the bench as a target.
5. **The `.bench/` state is the harness's alone**: plans, tasks, exchanges, memory, transcripts,
   `defaults.json`. The memory tool keeps writing it (that is the harness writing, at the model's
   request, into a store the model cannot list).

### 3.3 Reaching a workspace

Unchanged in mechanism, tightened in scope: `/v1/workspaces/{id}/tools` hands the bench the pod
address; the `allow-bench-tools` policy admits the bench pod; every ide call carries the bench
token. New: every call also carries **`tree`** (§4.4), and the bench sets it from the session — a
workspace session's tree is `main`, an agent session's is its own name — so **the session never
chooses**. A tool call that names a tree the session does not own is refused by the bench before it
leaves, and by the tool server if it ever arrived.

### 3.4 Probes

- `bench.no_hands`: a bench session asked to "cat /etc/hostname" answers that it has no shell and
  no tool does it; the transcript shows no tool call.
- `bench.pkg_needs_workspace`: "install jq" with no workspace → the refusal sentence, no proposal.
- `ws.tree_pinned`: a workspace session's ide call with a foreign `tree` in the arguments is
  rewritten to its own before it is sent (bench test), and the tool server 403s a foreign tree with
  a bench token that is not the tree's owner's (Rust test).

### 3.5 A session knows its tree, not the container

Owner (21:40 IST): "ensure that the session will work in its working directory and it's never
taught about the folder structure on the container. actually instruct to avoid."

- **Paths in and out are tree-relative.** A tool takes `src/main.rs`, never
  `/home/kl/workspaces/{ws}/src/main.rs`; a tool result, a listing, a process title, a diff header
  and an error message all print tree-relative paths. The tool server strips its root before
  answering, and refuses an absolute path with "paths are relative to your working directory"
  — a 400, not a 403, because there is nothing to be denied, only a shape to correct.
- **No environment leaks the layout.** `exec` runs with `cwd` at the tree root and `PWD=.`
  semantics in the prompt; `HOME`, `KL_WORKSPACE`, the pod name and the node are not in the exec
  environment the model can print (`env` shows the profile's `PATH`, `PORT`, `KL_PORT_RANGE`,
  `KL_TREE`, and what the person's shell rc sets, nothing of ours). `pwd` inside an exec answers the
  real path — an unavoidable leak of one string — and the identity tells the model that path is
  not to be reasoned about or reused.
- **The identity instructs.** One paragraph in every session identity (bench, workspace, agent):
  "You work in one working directory. Every path you give or receive is relative to it. Do not
  explore, describe or depend on where that directory sits on a machine, what is beside it, or how
  the machine is laid out; none of that is yours, and tools refuse it. If a task seems to need a
  path outside your directory, say so in your reply instead."
- **The bench session has no directory at all** and its identity says so in the same words:
  "You have no working directory. Name a workspace."


### 3.6 Every mutation asks, files included (owner, 04:00 IST 18 Sep)

The Yes/No card gates every change, not only platform writes: in a workspace or agent session
`write`, `edit`, `patch`, `exec` and a `process` start propose like `kl_*` does, with the same
three answers (yes · yes and don't ask again for this tool this session · no). Modes: **build** —
everything mutating asks; **accept-edits** — file tools run unasked, commands still ask; **plan** —
mutating tools refused. Reads never ask. The "don't ask again" memory is per session and per tool.


### 3.7 Bench = semantics, workspace = implementation (owner, 04:25 IST 18 Sep)

The bench session holds the semantic context — what exists, what it does, its contracts, its
state — and never the implementation. The workspace session holds the implementation — files,
code, commands, digests, ports. Every reply that crosses from a workspace to the bench is SHAPED
by the harness, not merely shortened: `outcome` (done / blocked / needs the person), the
`contracts:` line, `what changed for the person` in capability terms, `next`. Anything naming a
file, path, command, digest or line of code is dropped from the bench copy; the full text stays
with the workspace session and in the Queue card. `kl_workspace_progress` is one line per ask.


### 3.8 An ask reports its decision, then its result (owner, 04:40 IST 18 Sep)

"When I ask for a small change and push, the same message goes to the workspace session; it
decides the change and tells the main session it is going ahead with a specific change; then it
builds, and once built and pushed it informs the main agent that it is done." So: the bench
forwards the person's intent as stated, in capability terms, never rewritten into tool names. The
workspace answers an open ask with **reports** (`report {ask, kind, text}`): the first, `progress`,
is its decision ("going ahead with …") — relayed to the bench and the person as a one-line update
on the ask, never settling it; further `progress` lines as milestones land; the last, `done` or
`blocked`, settles the ask with the shaped reply of §3.7. The bench waits between reports; it does
not poll. Every message that crosses sessions — asks, reports, briefs — is caveman-terse: no
preamble, no restating what the receiver already holds, the person's words plus at most one line
the receiver cannot know (owner, 04:45 IST).


### 3.9 Nothing waits forever: one lifecycle for every handoff (owner, 05:00 IST 18 Sep: "cover all
kinds of scenarios and plan to avoid such stale stops")

Every handoff the bench makes — an **ask** to a workspace, an **agent** dispatch, a **proposal** or
**question** to the person, a **watch** on a process, a **wait** on `/v1` (create → ready) — is an
*exchange* with the same state machine, persisted, swept, and visible:

`queued → running → (progress)* → done | blocked | expired | cancelled`

Rules:

1. **Persisted, not in memory.** The exchange table is `exchanges.jsonl` folded on boot (today the
   `asked` map, watches and proposals live only in memory and a bench restart forgets them). After a
   restart every open exchange is re-checked, never assumed.
2. **Every state has a deadline.** `queued` longer than 60 s (the workspace session never took it) →
   re-deliver once, then `blocked: "workspace session did not pick it up"`. `running` with no
   `progress` for `ask_idle_secs` (default 10 min) → the bench asks the workspace session one line
   ("still on ask-5?") and shows "quiet for 10 min" on the card; no reply within 2 min → `expired`,
   the bench session is told, the person sees it. A proposal or question open past 30 min shows
   "waiting since …"; it never expires on its own (the person's call), but it is cancelled when the
   session is archived.
3. **A reply settles by intent, not by tag alone.** The `[reply <id>]` tag is the fast path; if the
   workspace's turn ends with an open ask and no tag, the turn's final assistant text is taken as
   the reply and the workspace is bounced once ("say `[reply <id>]`"). A reply for an already
   settled ask is appended as an update, never a new exchange. A workspace turn that ends in an
   error (model refused, no key, tool failure) settles the ask `blocked` with the error's plain
   sentence.
4. **Progress is part of the lifecycle** (§3.8): a `report progress` resets the idle clock and is
   shown on the card; the bench never polls.
5. **Watches are bounded**: a matched line is said once; a watch ends with its process; a watch
   fires at most `watch_max_fires` (20) times, then says so and stops.
6. **Duplicates are one exchange**: an ask with the same text to the same workspace while one is
   open joins the open one (the bench is told "already asked, ask-5").
7. **Sweep every 30 s** advances deadlines, reaps orphans (an exchange whose session is archived),
   and marks background tasks whose process is gone `lost`. Every transition is one line in
   `exchanges.jsonl` and one row in the Queue tab with its age; nothing is silent.
8. **On the fleet** `bench.ask.settles` (a reply without the tag still settles), `bench.ask.idle`
   (a workspace that goes quiet is nudged then expired) and `bench.restart.keeps_asks` (an open ask
   survives a bench restart) hold this.

## 4. Subagents work in trees, not workspaces

### 4.1 Decisions

- A subagent gets **no workspace pod**. It gets a **tree**: a writable snapshot of the workspace's
  main working directory, inside the same workspace container, as light as possible.
- The tree is a **nested btrfs subvolume** at `{ws}/.agents/{name}`. `btrfs send` skips nested
  subvolumes, so trees never travel with push, sync or replica; `.agents/` joins the global
  gitignore; caches inside arrive warm.
- The **node agent** cuts and deletes trees, on a `/v1` request. The tool server cannot (uid 1000).
- The workspace's **one `kl ide serve` serves every tree**. Every fs, edit, exec and process call
  carries a `tree` parameter; the main tree's confinement **excludes `.agents/`**, so the main
  session can never read a subagent's files.
- Packages installed while a subagent works land on the **workspace** (pod-wide profile); that is
  accepted.
- A subagent's session and tree **stay until its task is merged and clear**; nothing is dropped on
  failure — the person looks at what happened, then closes it.
- Ports clash: subagent and main tree share a network namespace. Handled in §4.6.
- The current agent-on-a-cloned-workspace path is **removed**.

### 4.2 The tree as a platform object

`Workspace.spec.trees: Vec<TreeSpec>` — written only by `/v1`:

```
TreeSpec { name: String /* [a-z0-9-]{1,32} */, from: TreeFrom::Main, created: Time }
```

`Workspace.status.trees: Vec<TreeStatus>`:

```
TreeStatus { name, path: "/home/kl/workspaces/{ws}/.agents/{name}", ready: bool, reason?: String }
```

Routes:

- `POST /v1/workspaces/{id}/trees {name}` → 202; 409 if the name exists, if the workspace is not
  Running (a tree is cut from a live tree, there is nothing to snapshot on a stopped one), or if the
  ceiling is reached (**8 live trees per workspace**, *proposed*, a `ClusterSettings` knob
  `trees_per_workspace`). Quota is not charged: a tree is bytes on a volume the owner already pays
  for and CPU in a pod already sized.
- `DELETE /v1/workspaces/{id}/trees/{name}` → 202; the spec entry is removed, the agent deletes
  the subvolume on its next pass, status drops the row when it is gone.
- `GET /v1/workspaces/{id}` lists `status.trees`.

The node agent's workspace reconciler gains one step after the pod is Running: for every spec tree
with no ready status, `btrfs subvolume snapshot {ws} {ws}/.agents/{name}` **excluding** the
`.agents/` directory of the source (a nested snapshot of a tree that itself contains nested
subvolumes yields empty directories for them — correct, and stated so nobody wonders), then
`chown -R 1000` is **not** needed (a snapshot keeps ownership). For every ready status with no spec
entry: `btrfs subvolume delete`. The orphan-voldir sweep learns `.agents/*` as a place a subvolume
may legitimately live, and deletes one whose workspace spec does not name it (crash between the
DELETE and the pass). `cleanup_parent` deletes every tree before the worktree, since a subvolume
with nested children cannot be deleted first.

Sync points, pushes and replicas are unaffected: `btrfs send` of the parent does not carry nested
subvolumes, so a replica of the workspace holds an empty `.agents/{name}` directory and nothing
else; a restore or clone starts with no trees. `history` records `tree.created`/`tree.deleted` as
transitions like any other.

### 4.3 The agent's lifecycle, end to end

1. A workspace session calls `kl_agent_run {brief, model?}` (today's tool, new mechanism).
2. The bench names the tree `{slug}-{4hex}`, `POST /v1/workspaces/{ws}/trees`, waits on
   `status.trees[name].ready` (bounded, the same wait the agent path used for a clone).
3. The bench creates an **agent session** (kind `ephemeral`, `workspace: ws`, new field
   `tree: name`), model from §1.2's rule, ide tools bound to `{ws pod}:7788` with `tree` pinned.
4. The subagent works in its tree; builds hit warm caches; ports per §4.6; it pushes its branch
   from inside the tree with the workspace's own git identity and keys (present in the pod today).
5. It reports to the calling session (direct line, no queue, as ruled earlier). Its status is one
   of the four the agents design names.
6. The tree and the session **stay**. The sidebar shows the tree nested under its workspace,
   labelled by the agent's name and status. The person opens it, reads the transcript, opens the
   diff, and when the work is merged and clear, closes it: `kl_agent_close` → `DELETE .../trees/
   {name}` and the session is archived. Closing is a **proposal** like every state change.

There is no `/v1 clone` per agent, no clone workspace, no second pod, no `based_on`.

### 4.4 `tree` in the tool server

Every tool and every `/fs/*` route takes an optional `tree` (default `main`). The server keeps one
`TreeCtx { root, graft, watcher }` per tree, created lazily on first use when `{ws}/.agents/{name}`
exists, dropped when it disappears. `paths::confine(root, home, given)` is called with **that
tree's root**; for `main` the root is `{ws}` and the confinement additionally refuses any resolved
path under `{ws}/.agents/` — the one place under its root the main tree may not look. A relative
path is relative to the tree; an absolute path must resolve under the tree; the home is **no longer**
an allowed absolute prefix for tools (it was, so a model could edit dotfiles; that is a shell's job
now and the shell is the person's). `exec` runs with `cwd` the tree's root and `KL_TREE={name}` in
the environment. `process_*` and `/stream/process/{id}` are tree-scoped: a process id names the
tree it was started in, and listing from one tree never shows another's.

Graft: one graph per tree, built on first use and refreshed by the same debounce; `/healthz`
reports per tree. The watcher watches each tree's root.

The bench token still authorises the whole pod; the bench pins `tree` per session (§3.3). The
server does not know which session called; it trusts the bench for that and confines for itself.

### 4.5 Sidebar and sessions

`SessionRow.tree?: string`. The workspace tree in the sidebar shows agent sessions nested under the
workspace, labelled `{agent name} · {status}`; a closed one leaves the tree. The Files tab of an
agent session reads `/fs/tree?tree={name}`; CHANGES shows the tree's own git status; a
"Diff against main" action shows `git diff main..HEAD` from inside the tree (an exec, read-only).

### 4.6 Ports

Both measures:

- **Range by convention.** The tool server assigns each tree a block of 100 ports from 20000
  upward in creation order (`main` has no block; it owns the normal range) and sets `PORT`,
  `KL_PORT_RANGE=20100-20199` in every exec in that tree. The agent brief the harness composes
  says: "Your ports are 20100–20199; the main tree owns the rest." Most dev servers honour `PORT`.
- **Refusal as the net.** `exec` with `detach: true` in a tree first parses a `--port`/`-p`/`PORT=`
  hint from the command line *(best effort)*, and on spawn the server watches the process's first
  `bind` failure line (`EADDRINUSE`) in the ring and marks the process `failed: port N in use by
  {tree or main}: {cmdline}` from `/proc/net/tcp` ownership, so the model gets a sentence rather
  than a stack trace. No kernel-level enforcement; a namespace per tree would be a pod restart.

### 4.7 Wrapping every exec in bubblewrap *(owner asked 21:45 IST: "can we actually wrap the
kl ide serve with something like bubble wrap?")*

Yes, and it is the right place: `paths::confine` guards what the **tools** touch, but an `exec` is
a shell, and a shell can `cd ..`. Wrapping each exec — not the server — closes that.

`kl ide serve` runs every `exec` and every detached process as:

```
bwrap --unshare-all --share-net \
      --die-with-parent --new-session \
      --bind {tree} {tree}   (the same path, so the leak in §3.5 stays one string) \
      --ro-bind /nix /nix  --ro-bind {profile} {profile} \
      --ro-bind /etc/passwd /etc/passwd  --ro-bind /etc/resolv.conf /etc/resolv.conf \
      --tmpfs /tmp  --proc /proc  --dev /dev \
      --setenv HOME {tree}/.home  (a per-tree dotfile dir, so git/npm/cargo config land in the tree) \
      --chdir {tree}  -- {command}
```

What the command sees: its own tree read-write, the Nix store and profile read-only, a fresh
`/tmp`, the pod's network (ports, §4.6, are unchanged), and nothing else — not the workspace root,
not other trees, not the home, not the token, not `kl`. `--die-with-parent` ties a detached
process to the server's lifetime the way the ring already does. The server itself stays outside
the wrapper: it must see every tree to serve them.

Owner (21:50 IST): no spike up front — build it, test it on the fleet, improve later. Two things
to watch when it lands: whether bwrap's user-namespace and `/dev` setup runs under the workspace
pods' runtime class (fallback `unshare -Urm` with bind mounts), and per-exec overhead (`ide.exec`
asserts under 50 ms).

`bubblewrap` joins `WS_BASE_PACKAGES` so it comes from the pin like everything else. The shell
sidecar (§2) does **not** use it: the shell is the person's, and its boundary is the container.

### 4.8 Probes

- `ws.tree.cut`: POST a tree → ready within 10 s; `/fs/tree?tree=x` lists the source's files;
  `read` of a path under `.agents/` from `main` → 403 naming the path.
- `ws.tree.isolated`: `write` in tree x, `read` the same path in `main` → unchanged.
- `ws.tree.no_travel`: push the workspace, restore it elsewhere → `.agents/x` empty.
- `ws.tree.ports`: exec in tree x prints `$PORT` in its block; a detached listener on a port main
  holds → `failed: port … in use`.
- `ws.tree.closed`: DELETE → gone from status and disk within one pass; `cleanup_parent` with a
  live tree succeeds.
- `agent.tree.run` (bench): dispatch → tree, session, report, tree stays; close → proposal → gone.

## 5. Boundaries, stated once

For a reviewer or a probe writer, the invariants this design must hold, in the order they are
most likely to be broken by a later convenience:

1. **No tool reads a path where the model runs.** The sessions container has no registered
   filesystem, shell or process tool in any mode. `tool_search` cannot find one because none is
   registered.
2. **Every ide call names a tree, and the session did not choose it.** The bench pins `tree` from
   the session record; the tool server confines to it; `main` excludes `.agents/`.
3. **The shell has no route to code, tools or tokens.** Separate container, home-only mounts,
   `ttyd` only, no token; the tool server 401s it.
4. **The person's pick is the default; a dispatch's pick is that session's.** Nothing else moves
   the default.
5. **A tree is the agent's only workspace.** No pod, no clone, no `/v1 clone` from the agent path.
6. **Trees never travel.** Nested subvolumes, skipped by send; `.agents/` globally ignored.
7. **Nothing is dropped on failure.** A failed agent's tree and session stay until the person
   closes them.

## 6. Out of scope

Provider credential flows beyond API keys (OAuth, env-based providers); phases; terminal
persistence or reconnect; shell processes anywhere in the UI; a tree from anything but `main`
(a tree of a tree is refused); per-tree resource limits; merging a tree's branch from the UI (the
agent pushes, the person merges where the repo lives).

## 7. Order of work

Three slices, each shipping alone, spec by Fable 5.1, plan by Fable 5.1, implementation by Opus
5.1 per the owner's process:

1. **§1 picker** — harness only. Session fields, defaults, `/model`, `Ctrl+T`, footer.
2. **§2 + §3 shell sidecar and hands-free sessions** — Rust (pod specs, shell image, netpol, token
   file mode) + harness (drop `ownTools`, bash, shell-gate; tunnel splice to 7790; xterm over ttyd;
   retire pty persistence). Pods recreated by hand, no backfill.
3. **§4 trees** — Rust (CRD fields, routes, reconciler step, sweep, `tree` in the tool server,
   ports) + harness (agent path on trees, `tree` pinning, sidebar, Files/diff). Removes the clone
   path.

## 8. What this retires in earlier specs

- `2026-09-17-bench-tools-no-fs-design.md`: the bench's own-workspace ide tools (`ownTools`), the
  shell gate, "install X with no workspace named means the bench", agents on cloned workspaces.
- `2026-09-17-terminals-tmux-and-tabs-design.md`: §2b tab reconcile, §3 reconnect, §7 tool-server
  PTY persistence, `/stream/pty*` routes. The xterm option choices (§4) stay.
- `docs/capacity-model.md`: gains the shell container (requests 50m/64Mi, *proposed*) per pod.
