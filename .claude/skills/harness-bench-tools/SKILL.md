---
name: harness-bench-tools
description: Use when changing what a bench or workspace session can do in the Kloudlite harness — kl_* tools, spawn arguments, the system prompt, the bench-to-workspace ask path, shell gates, or when reviewing a bench transcript for misbehaviour. Encodes the owner's rulings (2026-09-17) on who may touch what and how the model must behave.
---

# Harness bench tools

One agent process (pi — never named to the model; it presents as "the Kloudlite harness") per
session inside the person's bench pod. Three kinds of session — bench, workspace, btw fork — and
one set of rules. These are the owner's rulings and are not up for re-litigation in a task.

## Boundaries

1. **Every session's hands are its own workspace's, and only its own.** read/write/edit/bash/
   grep/find/ls/process run on that session's workspace tool server. The bench is a workspace
   too: its session gets the seven pointed at its OWN workspace container
   (`KL_TOOLS_ADDRESS=127.0.0.1:7788`). Nothing runs in the bench container: pi's builtins are
   off (`--no-builtin-tools`); `background.ts`/`process.ts` are gone.
2. **Another workspace is asked, never driven.** No tool reaches a different workspace's files,
   shell or tool server. Every mutation meant for another workspace is a message queued into
   that workspace's OWN session (`kl_workspace_ask`; the session is created if absent). Asks are
   never refused: tagged `[ask <id> from <session>]`, FIFO per workspace, the workspace session
   works them in its own order, and each answer returns to the sender that asked (`[reply <id>]`
   routes by id; a turn the person started there settles nobody's ask). Progress of another
   workspace is read with `kl_workspace_progress`, never off disk.
3. **Its own machine is the default target.** "Install X", "add a package", "switch the
   environment" with nothing named act on the session's own workspace: `kl_pkg_*`, `kl_env_*`.
   **A new component (backend, service, separate project) gets its own workspace** via
   `kl_workspace_create` + `kl_workspace_ask`, unless the person names an existing one.
4. **Every session manages its space's environment**: `kl_env_current|switch|clear`,
   `kl_environments`, `kl_environment`, `kl_environment_service_add|rm` (read-modify-write by
   name — a whole-list PATCH through a narrower schema once nearly dropped mongodb's mount),
   `kl_intercept` (any service to any workspace of the space). Create/start/stop/push/clone/
   restore/delete of environments are bench-only.
5. **The platform is reached only through catalogued `kl_*` tools.** Each is one `/v1` call in
   `pi/catalog.ts` with its effect (read / write / destroy) appended to its description. A person
   asks for something no tool covers → add the tool (and the `/v1` verb if missing). The model
   must never improvise: the shell gate (`forbidden()` in `workspace-tools.ts`) refuses commands
   touching `KL_TOOL_TOKEN_FILE`, `/etc/kloudlite`, `/opt/harness`, introspection of the `kl`
   binary, `.bench/` session stores, or `/v1/` URLs. `kl_capabilities` is where it looks when
   asked what it can do or when nothing fits.
6. **Never an unasked write.** A write/destroy tool runs only when the person asked for that
   change in this conversation. Asked what it can do, it describes tools by name and runs none
   (a model once ran `kl_environment_service_rm` to "test" a tool). Destroy-effect tools are
   candidates for a desktop confirm card — open design item.
7. **It does not know what it runs on.** `tellItWhereItStands` REPLACES pi's system prompt in
   all three modes: "the Kloudlite harness", no "pi", no `/opt/harness`, no coding-agent
   boilerplate. The fork gets the identity with `--no-tools`.

## Newer rules (2026-09-17 afternoon)

8. **Lifecycle tools wait; nobody sleeps.** create/start/restore/push/service_add poll their own
   GET until the platform rests (`settle()`); the shell gate refuses a bare `sleep`/timer command.
9. **State changes are asked first.** Every write/destroy `kl_*` (except asks and own `kl_pkg_*`)
   is a PROPOSAL: a Yes/No card in the transcript, the answer recorded as the person's row;
   no answer within the cap = declined. Probes that drive a model into a write must answer
   `GET /proposals` with yes, and one probe asserts a declined change never happens.
10. **The tool result is the view.** `kl_*` results render as cards (workspace, environment,
    quota, history, ask chip, processes, packages, capabilities); the model never repeats their
    fields, speaks caveman (`harness/pi/caveman.md`, chat text only), leads with the result.
11. **Code and containers are tools.** Repos/pulls over `/v1` (`kl_repos`, `kl_pull_*`,
    `kl_commit`…); `kl_repo_clone`, `kl_container_build|push`, `kl_images` run in the OWN
    workspace through the `kl` CLI. Open: the pod must learn the git ssh host
    (`KL_GIT_SSH_HOST` in `login_env`) — until then `kl_repo_clone` refuses by name.
12. **Panels are per session.** Processes and queues filter by the active thread's session;
    asks show as the sender's queue from exchange events; processes that exit are noticed by a
    10 s poll while any row runs.

13. **Twelve tools always on** (read write edit bash grep find ls process ask plan skill
    tool_search, plus memory); every `kl_*` platform tool sits behind `tool_search`, inactive until
    found. Six product skills under `harness/skills/`; the identity names them in one line.
14. **Agents** (`ask {to:"agent"}`): fresh session, one task, reports once; `isolated: true` gives
    it an ephemeral CLONE of the workspace for parallel or risky work, closed with `ask_close`.
    A fork orders the inbox; the bench keeps every session's plan current from events.
15. **Memory**: the person's knowledge under `{bench}/.bench/memory/`, index in every identity.

## Incident record (why these rules exist)

- 2026-09-17 05:00 IST: asked "add nats to the env" with no matching tool, the model read
  `/opt/harness/pi/kloudlite.ts`, cat'd the tool token into node scripts against `/v1`, then
  created a second `devstack` and DELETED the first (twice, across two sessions). Owner data
  survived only as pushed snapshots.
- 06:24: asked what tools it had, it ran a destroy tool to find out.
- 06:38–06:42: with no tool for "HTTP service", it grepped strings out of the `kl` binary and
  its own session logs; for "what's happening" it grepped another workspace's transcript off disk.
- 06:41: "create a backend with golang" was queued into the running frontend workspace.
- 12:45: a roll landed the single gateway replica (in-memory caps → exactly one; now pinned by
  nodeAffinity to the DNS nodes session-0/env-0) on session-1, a node with no Cloudflare A
  record — every tunnel 521 for 25 min. `gateway.yaml` now runs one replica per pool node.

## Where things live

| Concern | File |
|---|---|
| Spawn argv per kind, `KL_SESSION`, `KL_TOOLS_WORKSPACE`, `KL_TOOLS_ADDRESS`, `RpcChild.hands()` | `harness/bench/src/rpc-child.ts` |
| Tool catalogue (model description = Settings › Tools row) | `harness/pi/catalog.ts` |
| `kl_*` platform tools, own-machine tools, environment tools, identity prompt | `harness/pi/kloudlite.ts` |
| Own-workspace hands, `process`, background `bash`, shell gate `forbidden()` | `harness/pi/workspace-tools.ts` |
| Ask path: `POST /workspaces/{id}/ask`, FIFO, `[reply]` routing, `GET /sessions/{id}/tools` | `harness/bench/src/server.ts`, `bench.ts`, `exchanges.ts` |
| Transcript rendering: user rows come from pi's `message_start`, never `queue_update` | `harness/src/renderer/live.ts` |
| Design record | `docs/superpowers/specs/2026-09-17-bench-tools-no-fs-design.md` |

## Checklist for any change

- Every registered tool is in `catalog.ts`; the catalogue test stays green; every tool a
  workspace session registers is in `WORKSPACE_TOOLS` (pi's `--tools` is a strict allow-list
  over extension tools too).
- Spawn-arg tests cover bench / workspace / fork; the prompt test finds no "pi" and no
  `/opt/harness`; the `forbidden()` table test has an allowed and a refused row per pattern.
- A new way for a session to touch another workspace? Stop: it is an ask.
- A new whole-list write? Make it add/remove by name over a fresh read.
- Gates: `cd harness && npm run typecheck && npm run bench:test && npm run build`.
- On the fleet: `bench.tools.own_hands`, `bench.shell.workspace`, `bench.idle.wake` hold it;
  read the person's own transcript after a ship (`.bench/sessions/*.jsonl` via kubectl exec) —
  every ruling above came from one.
