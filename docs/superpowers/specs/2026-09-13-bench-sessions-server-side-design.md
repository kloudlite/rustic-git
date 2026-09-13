# Bench sessions on the server

**Status:** draft for review · **Date:** 2026-09-13 · **Scope:** the harness's bench — its sessions, their messages, the messages exchanged between sessions and workspaces, background tasks and processes — runs on the platform instead of the laptop, so every device sees the same bench and it survives any one of them.

## The problem

Today the harness runs `pi --mode rpc` on the laptop. Everything the bench is — pi's session files, the list of sessions, background commands, long-lived processes, the exchange log between sessions and workspaces — lives on that one machine and dies with it. Closing the lid stops a background build; a second laptop sees a different bench; nothing the bench did is visible anywhere else.

## What a bench session is

**A bench session belongs to the person.** It is how a person reaches into, and writes into, many workspaces at once: the conversation, the plan, the messages it sends to each workspace and the answers it gets back are that person's work. A team is where the session operates — which workspaces it can reach, which region it runs in — never who owns it. Nobody else reads a person's sessions, and nothing about a session is the team's to keep, prune or hand over.

## The two decisions everything hangs off

1. **The session runs remotely, in one place.** A `Bench` object — owned by the person, one per team they work in — whose pod runs pi. No device runs a session; every device is a view of the pod over the gateway tunnel, speaking pi's RPC exactly as the harness speaks it to a local child today. One process runs a session, so every device is consistent by construction, not by sync.
2. **The record is files on the region's NFS share: one folder per person per team**, `benches/{team}/{person}/`, beside the `homes/{person}/` directories the agents already mount. pi writes its own JSONL sessions there unchanged; the harness writes its list, the exchange log and the process table next to them. No database, no new storage tier, no change to how pi persists.

Why NFS files rather than a database:

- **pi already persists to files.** Pointing `--session-dir` at the NFS folder is the whole integration: no `SessionManager` subclass, no dependency on pi's private write methods, no hydrate-on-open. Sessions are grep-able, diff-able, and pi's own tools (`/resume`, `--fork`) work on them.
- **One writer per folder.** Exactly one bench pod runs for a `(team, person)`, and it is the only process that writes that folder. The consistency a database would give comes from the single writer, which the `Bench` object already guarantees.
- **The share is already there, mounted, and operated.** The agents mount the region's export at `{pool}/homes` (`mount_homes`, `WS_HOMES_EXPORT`); benches are a sibling directory on the same share, mounted the same way.
- **Per team, per person.** A team's benches are separable (move, back up, delete a team's folder) and a person's folder in one team is not readable from another team's bench pod.

**A team is bound to a region**, so a team's benches have exactly one share to live on: the team's region's. There is no cross-region case to design for — a bench never needs to follow a person anywhere its team is not.

The cost, stated: there are no server-side indexes; views are computed by reading append-only logs, which is fine at one person's scale.

## What runs where

Every session a person holds in a team runs in that team's bench pod. Only tool calls leave it.

| Session | Where pi runs | Where its tools run | Its record |
|---|---|---|---|
| Bench session | bench pod, one pi per open session | bench pod: `bash`, files, background tasks, `process`, `kl_*`, as the person (`/home/kl`) | `/bench/sessions/*.jsonl` |
| Workspace session (the workspace's thread) | bench pod | that workspace's tool server, `kl ide serve` on port 7788 of its pod | `/bench/workspaces/{ws}/thread.jsonl` |
| Ephemeral agent session | bench pod | the ephemeral's own tool server (an ephemeral is a workspace cut for one agent) | `/bench/workspaces/{ws}/eph/{id}.jsonl`, under the workspace it was cut from |
| `/btw` | bench pod: a read-only fork that exits after its one answer | bench pod, read-only tools only | `/bench/btw/{session}/{id}.json` |

- **The conversation never lives in a workspace.** Messages, queue and history are in the bench pod and the bench folder, so stopping, restarting or deleting a workspace loses none of its conversation. A tool call against a stopped workspace returns an error naming its state, and the session carries on.
- **`workspace-tools`**, a pi extension, gives a workspace or ephemeral session pi's own `read`, `write`, `edit`, `bash`, `grep`, `find` and `ls`, each a `POST /tools/{read|write|edit|exec|grep|glob}` on the target's tool server (`crates/ide`). Such a session starts with `--tools read,write,edit,bash,grep,find,ls`, so no bench tool — background tasks, `process`, `kl_*` — is within its reach. Background tasks and processes inside a workspace are not in this cut; the tool server's `exec` with `detach` is where they go.
- **The bench reaches the tool server on the pod IP.** `kl ide serve` binds `0.0.0.0:7788` instead of loopback. The network fence is the namespace: a person's bench and their workspaces in one team share `ws_namespace(owner, team)`, whose default deny admits only same-namespace pods, and an explicit `allow-bench-tools` policy names the bench → workspace:7788 grant. The one existing way into a workspace pod from outside its namespace, an intercept's ingress, is narrowed to the intercepted ports. The ssh tunnel (`kl-connect ws ide`) reaches loopback as before.
- **The bench learns the address only from `/v1`.** `GET /v1/workspaces/{id}/tools` answers `{address}` to the workspace's owner alone, while it is Ready. The extension dials nothing it was not given there, and asks again after a connection error, because a restarted pod has a new IP.

## Objects

### `Bench` CRD (`kloudlite.io/v1alpha1`, cluster-scoped)

```yaml
spec:
  owner: karthik1729          # the person; spec.owner is the truth, labels are a view
  team: kloudlite             # a person's own handle for the personal bench
                              # no region field: a team is bound to a region, and the bench is placed in the team's
  image: ghcr.io/kloudlite/kloudlite-bench   # pi + kl + harness-bench + the extensions, pinned per release
  model: deepseek/deepseek-v4-flash          # default; a session may set its own
  desiredState: Running | Stopped
  resources: { cpu: "1", memory: "2Gi" }     # counted against the person's Quota while running
status:
  nodeName, phase, conditions[Ready, Placed, FolderReady], podRef
```

- Exactly one per `(owner, team)`; `/v1/bench` creates it on first use. Its region is the team's region, read from the team at create and never stored on the bench, so a bench cannot disagree with its team about where it lives.
- Its pod is the workspace pod's shape without the btrfs volume: the NFS home at `/home/kl` (config, provider keys, the CLI token), **the bench folder at `/bench`**, the `user-key` Secret, and the `resolv.conf` attach mechanism so a bench can be attached to an environment. It holds no working copy; code lives in workspaces, and every session the person holds in that team runs here (see What runs where).
- Reconciled by the node agent like a workspace (`controller/bench.rs`), placed by the same claim rule.
- **`Stopped` stops the work, not the history.** The pod keeps running `harness-bench --read-only` with no pi processes, no tools and no model: a small reader (`resources: { cpu: "50m", memory: "128Mi" }`) that serves the session list, every session's messages, the exchanges and the workspace threads through the same tunnel. Reading it uses pi's SDK directly — `SessionManager.list(cwd, "/bench/sessions")` for the list and `SessionManager.open(file).getEntries()` / `buildSessionContext()` for a transcript — which read the JSONL without starting an agent. `Running` starts pi for each open session in the same pod. Only deleting the `Bench` removes the pod; the folder is untouched either way.
- Parks `Creating/FolderNotReady` until the node has the share mounted, exactly as a workspace parks on `HomeNotReady` — a pod must never start on an empty local directory where the folder should be.

### The bench folder — `{team's region share}/benches/{team}/{person}/`

Created by the agent (`ensure_bench_folder`, `mkdir` + `chown` to uid 1000, safe on every reconcile — the `ensure_shared_home` shape). Mounted into the pod at `/bench` as a hostPath of the node's mount; the pod sees its own folder and nothing above it.

```
/bench/sessions/*.jsonl                    pi's own session files (pi --session-dir /bench/sessions), unchanged format
/bench/sessions.json                       the list: [{id, name, seq, file, created, lastActive, archived, model, kind, workspace, target}]
/bench/exchanges.jsonl                     append-only: every message between a session and a workspace
/bench/workspaces/{ws}/thread.jsonl        pi's session file for the workspace's thread, run in the bench pod
/bench/workspaces/{ws}/eph/{id}.jsonl      pi's session file for an ephemeral agent cut from that workspace
/bench/btw/{session}/{id}.json             a /btw answer: {question, entries, at}
/bench/tasks.jsonl                         append-only: background task transitions
/bench/procs.json                          live process table: [{id, session, name, command, pid, started, ended?, code?}]
```

- **Session messages** are pi's JSONL, written by pi. The harness never edits one.
- **Messages across sessions and workspaces** are one log, `exchanges.jsonl`, one line per event: `{ts, id, session, workspace, dir: in|out, text, state, ref}`, plus `{ts, id, state}` lines for transitions and `{ts, session, discarded: true}` when a session is deleted. A session's queue and a workspace's queue are two filters over the same log, so they cannot disagree. `harness-bench` keeps both views in memory (built at start by reading the log once) and serves them.
- **Workspace and ephemeral sessions** are pi JSONL too, written by the pi `harness-bench` runs for them in the bench pod, and kept under `workspaces/` so a workspace's conversation outlives its pod. `sessions.json` lists them with `kind` (`bench` | `workspace` | `ephemeral`, absent meaning `bench`), `workspace` (the workspace the thread belongs to) and `target` (the workspace whose tool server runs its tools: the workspace itself, or the ephemeral).
- **Append-only, one line per write, `O_APPEND` + `fsync`**: a crash mid-write loses at most the line being written, never earlier lines; a torn last line is skipped on read. `sessions.json` and `procs.json` are small and replaced atomically (write a temp file, `rename`).
- **Delete** removes the session's entry from `sessions.json`, its `btw/`, and appends the discard line; pi's JSONL is moved to `sessions/.trash/` (removed by prune, not immediately). **Archive** is the flag.

### The bench process — `harness-bench`

One supervisor in the pod (the successor of the harness's `src/pi.ts`):

- **Per open session**: `pi --mode rpc --session-dir /bench/sessions --session <file> -e background.ts -e process.ts -e kloudlite.ts`, cwd `/home/kl`. The same process the laptop runs today, with a different directory.
- **Per workspace or ephemeral session**: `pi --mode rpc --session /bench/workspaces/{ws}/thread.jsonl` (or `eph/{id}.jsonl`) `-e workspace-tools.ts --tools read,write,edit,bash,grep,find,ls`, with `KL_TOOLS_WORKSPACE={target}` and `KL_TEAM`. Its tools run in the workspace; nothing else about it differs from a bench session.
- **Extensions write the folder**: `kloudlite.ts` appends to `exchanges.jsonl` for every call that hands work to a workspace and every result that comes back; `background.ts` appends `tasks.jsonl`; `process.ts` maintains `procs.json`.
- **Surface** on port 7789 of the pod IP (`--host 0.0.0.0`), admitted only from the gateway by the `bench-ingress` NetworkPolicy: the gateway dials the pod IP, and a bench pod has no sshd to forward through. `harness-bench --ping` is the pod's readiness probe.
  - `WS /sessions/{id}/rpc` — the session's RPC, one JSONL frame per message; the harness's `Pi` class becomes a WebSocket client.
  - `WS /events` — every session's events plus list, exchange, task and process changes, fanned out to every connected client.
  - `GET /sessions`, `POST /sessions`, `DELETE /sessions/{id}` (`{stop: true}` applies the in-flight warning: abort, cancel tasks, stop processes, then delete), `/archive`, `/restore`.
  - `GET /sessions/{id}/messages?after=&limit=` — pi's `get_messages` for an open session, the JSONL parsed otherwise (an archived session is readable without waking it).
  - `GET /exchanges?session=|workspace=&after=`, `GET /tasks`, `GET /procs`, `POST /sessions/{id}/btw`.
  - `POST /workspaces/{ws}/session` and `POST /workspaces/{ws}/eph/{id}/session` open a workspace's or an ephemeral's session, idempotently. `GET /workspaces/{ws}/messages` and `GET /workspaces/{ws}/eph/{id}/messages` read its transcript. Its RPC is `WS /sessions/w-{ws}/rpc` or `/sessions/e-{id}/rpc`, like any session's.
- **Restart-safe**: on start it reads `sessions.json`, reopens every non-archived session — bench, workspace and ephemeral — with `--session <file>`, rebuilds the exchange views from the log, and marks `procs.json` rows without a live pid `ended: lost`.
- **Single writer, enforced**: at start it takes an exclusive lock file `/bench/.lock` (`flock`); a second bench pod for the same folder — a stale pod on a dead node, a double placement — cannot start writing: it exits 75 and writes the holder to its termination message, which the agent reports as `FolderLocked`. The lock is the folder-level fence the `Bench` object's single placement relies on.

### `/v1` additions (api tier, session JWT)

```
GET    /v1/bench?team=                 the caller's bench for that team (404 → none yet)
POST   /v1/bench                       create (idempotent per person+team), start
POST   /v1/bench/start | /stop
POST   /v1/bench/session               60 s single-purpose tunnel JWT (mirrors /v1/workspaces/{id}/ssh-session)
POST   /v1/bench/attach {environment} | /detach
GET    /v1/workspaces/{id}/tools?team=    a workspace's tool server address, to its owner only, while Ready
```

Everything about sessions and messages is `harness-bench`'s surface behind the tunnel; the api never reads the folder.

## Auth

- **Clients**: the person's session JWT for `/v1/bench*`; the tunnel JWT for the pod.
- **Folder isolation**: the pod mounts only `benches/{team}/{person}` — the same hostPath-of-a-subdirectory confinement homes use; `model::validate_mount` checks the path like every mount. The folder is the person's: owned by their uid, readable by their bench and nobody else's.
- **Reaching workspaces**: membership is checked where it matters — on every call a session makes into a workspace (`may_act(person, team)` on `/v1/workspaces/*`, as today), and at bench start. A person removed from a team can no longer start that team's bench or reach its workspaces; their sessions remain theirs — readable and exportable by them, deletable by them — and are never readable or prunable by the team.
- **pi's tools** act with the person's credential (the CLI token `/kl-login` writes to the home).
- **Bench → workspace tools**: the tool server has no credential of its own and gets none. Two things fence it. The network: the bench pod and the person's workspaces in that team share one namespace; `allow-bench-tools` admits bench pods to workspace pods on 7788, and an intercept admits its environment only on the intercepted ports. Ownership: `GET /v1/workspaces/{id}/tools` gives the address only to the caller who is `spec.owner`, only while the workspace is Ready, and refuses a workspace of another team than the bench's (`?team=`). Another person's workspace in a shared team is a 404, even to a team admin.
- **Provider keys**: `~/.pi/agent/auth.json` in the home; never in the bench folder, never in a Secret we mint.
- **Reads and writes are the person's alone.** No team role — member, admin, owner — and no platform surface (the superadmin console, `/admin/*`, history, monitoring) reads, lists, moves or deletes a person's sessions. There is no share, no audit view, no "view as". The api exposes only the person's own bench to the person's own token, and no admin router has a bench route at all.

## Sync across devices

Falls out of decision 1: one pod runs every session, every device is a view.

- **One source, many views.** `harness-bench` fans each RPC event out to every client of that session; a prompt typed on the laptop streams onto the phone; `^B` on one shows the task on the other.
- **The list and the messages are live.** `/events` carries session, exchange, task and process changes.
- **Writes are serialised in the pod.** Prompts go through pi's queue (steer / follow-up) in arrival order; list edits go through `harness-bench`; one process writes the folder. No merges, no conflicts.
- **Per device, deliberately:** open tabs, panes, the active pane, folds, theme — kept in the client's localStorage keyed by bench. The session list and remembered files leave localStorage.
- **Offline:** a read-only cache of the list, the last messages of open sessions and recent exchanges; the composer says "not connected"; reconnecting pages what it missed with `after=`.
- **Stopped bench:** every device still reads the full history — list, messages, exchanges, workspace threads — from the read-only pod; only sending needs `Running`, which is one click and keeps the same pod.

## What the harness stops doing

| Today (laptop) | After |
|---|---|
| spawns `pi` as a child | tunnel + WebSockets to `harness-bench` |
| `localStorage["harness.sessions"]` | `/sessions` + `/events` |
| remembers session files in `userData` | nothing; `/bench/sessions.json` is the record |
| tasks / processes from local events | same events via `/events`; `/tasks`, `/procs` on connect |
| exchanges: a fixture in `model.ts` | `/exchanges?session=` and `?workspace=` |
| `/btw` spawns a second local pi | `POST /sessions/{id}/btw`, answer under `/bench/btw/` |
| workspace and ephemeral threads: fixtures in `model.ts` | live sessions on the bench: `POST /workspaces/{ws}/session`, `GET /workspaces/{ws}/messages` |

`live.ts` keeps its shape: `thread(id)` still folds RPC events into messages; only where they come from changes.

## Failure behaviour

- **Tunnel drops**: the bench keeps working; the harness reconnects and pages missed messages; a prompt sent while disconnected is refused, never queued on the laptop.
- **Pod rescheduled**: pi processes are gone; the new pod takes the lock, reopens every session — workspace and ephemeral sessions included — from its JSONL, and the model continues from the last persisted turn; a tool call in flight is recorded as interrupted; processes and background tasks are marked `lost`.
- **Workspace stopped or unreachable during a tool call**: the call returns an error and pi continues with it as the result. The error names why: `/v1`'s 409 (`workspace api is stopped; start it to run tools`), or `workspace api did not answer at {address}` after one fresh lookup. The conversation is in the bench folder and is untouched. A job the tool server was running when its pod went is lost with the pod.
- **Node dies holding the lock**: `flock` is released when the node's NFS client lease expires; until then the new pod exits 75 on start and restarts, shown as `Ready=False/FolderLocked` with the holder's node in the message — never two writers.
- **Share unavailable**: the pod parks `FolderNotReady`; a running pod whose writes start failing stops accepting prompts and says so, rather than keeping work only in memory.
- **Region**: none to handle — a team is bound to a region, so its benches, their folders and the workspaces they talk to are all in that one region.

## Migration

`harness bench import`: copies the laptop's `~/.pi/agent/sessions/<harness cwd>/*.jsonl` into `/bench/sessions/`, merges `localStorage["harness.sessions"]` into `sessions.json`, through the tunnel, once; the local list is marked imported. Session ids and file names are preserved, so a re-run is a no-op.

## Verification (fleet, before "shipped")

- Unit (`harness-bench`): append + torn-line recovery, atomic replace of `sessions.json`, exchange views rebuilt from a log (both views agree; a discard line removes a session's rows from the workspace view), lock contention (second instance exits 75 naming the holder), each pi tool mapped to its tool-server call and a refusal returned as only its error, a workspace session's file under `workspaces/{ws}/` surviving a restart.
- Agent (`bins/agent/tests/reconcile/bench.rs`): folder created with the right owner, `FolderNotReady` without the mount, pod spec mounts only its own folder. Workspace pod (`k8s/tests`): the prelude binds the tool server on the pod IP, `allow-bench-tools` admits only bench pods on 7788, an intercept's ingress names only its ports.
- SLO ids in `deploy/slo.md`: `bench.create`, `bench.start.p95`, `bench.tunnel`, `bench.session.roundtrip` (create a session, prompt with no tools, read it back), `bench.exchange.both_views` (record an exchange; read it by session and by workspace), `bench.two_clients` (two WebSockets on one session see the same events in the same order), `bench.survives.reschedule` (weekly: delete the pod, expect every session reopened and processes `lost`), `bench.workspace.tool_roundtrip` (a workspace session on the bench runs `exec echo` in a workspace through its tool server, and the turn lands under `/bench/workspaces/{ws}/`).

## Open questions

1. **Leaving a team.** A person's sessions stay theirs after they leave. They keep a read-only bench for that team (the `--read-only` pod, never `Running`) — or is an export at departure enough?
2. **Quota.** Sessions are the person's, the pod runs in the team's region against the team's workspaces: does the bench pod's cpu/memory count against the person's `Quota` or the team's?
3. **Idle reader cost.** A read-only pod per person per team is small but always on. Scale it to zero after a day without a client and cold-start it on the next read (a few seconds), or keep it up?
4. **Retention and size.** Archive after 24 h idle (as now); prune archived sessions and `.trash/` older than 90 days on request only. Does the folder count against the person's `diskGb`? Only the person prunes.
5. **Search.** Rows to ClickHouse from the admin history consumer for "find where I discussed X" — a view, never the record. This cut or later?
6. **Personal bench region.** A personal "team" is the person's own handle — which region does it bind to: the person's default region, or chosen at first use?
