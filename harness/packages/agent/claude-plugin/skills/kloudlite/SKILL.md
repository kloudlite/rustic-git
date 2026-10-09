---
name: kloudlite
description: You run on Kloudlite; "workspace", "environment", "service", "snapshot" in a request mean Kloudlite's, driven by the workspace_*/env_*/service_*/intercept/packages_* tools ("delete all workspaces" = list them with workspace_list, then workspace_delete each). Read this before any task that creates, changes, stops, deletes, lists or works inside a workspace or environment, or mentions intercept, push, restore, clone, package or build: concepts, permission cards, and the team's conventions. Your role (main or workspace session) has its own skill beside this one; read both.
---

# Kloudlite

Kloudlite gives each person cloud dev machines (workspaces) and shared running stacks (environments) on one cluster. You drive it through the platform tools. "Delete all workspaces" means Kloudlite workspaces; only the main session creates and deletes them (list with `workspace_list`, then delete, following the convention for destructive verbs below).

## Two roles

Every session has exactly one role, and one skill says what that role does. You were given yours beside this one: `main-session` or `workspace-session`. Your first message also names it (`[role: ...]`). Read your role skill first; this skill is only the shared ground.

| Role | Lives in | Job |
|---|---|---|
| **Main** | the bench; no workspace, no source code | talks to the person; creates, clones, starts and deletes workspaces and environments; keeps the task board; hands each task to a workspace with `workspace_ask` |
| **Workspace** | one workspace's pod, `~/workspace` | owns one component: its design, code, service, intercept and working branch; reports to main with `main_tell`; may stop its own workspace when done |

Every session is isolated: it works only in its own folder and pod. No session reads, runs or changes code in another workspace, pod or session. Work crosses between sessions only through `workspace_ask` (main to a workspace, with a task id when the work is on the board), `main_tell` (a workspace to main: done, blocked or need) and the answers to asks. Workspaces never message each other; main carries facts between them.

## Concepts

**Workspace.** One dev machine: a pod with its own home (`/home/kl`, a btrfs volume), source under `~/workspace`, and its own installed packages. One workspace per component: frontend, backend, worker, test suite. Components can come from different repos or the same one. Each kind tends to need different packages.

**Worktree.** An extra working copy inside a workspace (`worktree_add` / `worktree_drop`), for a second branch without a second workspace.

**Environment.** A shared, multi-service stack for a team (the backend, its database, a queue...). Each service runs as its own StatefulSet. Add, change and remove services with `service_add` / `service_update` / `service_remove`.

**Service image.** When you add a service, you choose the image: build one with the `container_build` tool (`tags: ["name:tag"]`, context in your working directory; it builds on your builder and pushes to the Kloudlite registry) and retag with `container_push`. `kl` is not on PATH inside `exec`; these tools run it for you, or use any public image.

**Intercept.** Route one service of an environment to a workspace instead of the service's own pod. Traffic for that service then reaches the code running in your workspace. `intercept` starts it, `release` ends it. Only one workspace can intercept a service at a time. If the workspace stops, the intercept is released after a short grace period, but the wish stays until `release`.

**Space.** `space_env_current` / `space_env_switch` / `space_env_clear`. A team's space follows one environment, so its workspaces can reach that environment's services by DNS name.

**Snapshots.** A read-only copy of a workspace's or environment's disk.
- *Sync points* are cut automatically while a workspace runs. They are for crash safety only and are never shown as history.
- *Push* (`workspace_push` / `env_push`) records a named snapshot in history. Pushes are never pruned. This is not `git push`.
- *Restore* (`workspace_restore`, `env_restore`) makes a new copy from a snapshot. `env_restore_in_place` rewrites the environment itself.
- *Clone* (`workspace_clone`, `env_clone`) copies a workspace or environment. The reply's `based_on` says how old the copy is.
- `volume_list` / `volume_history` / `volume_refs` read the chain; `snapshot_delete` and `volume_delete` remove history.

**Interrupted workspace.** If a workspace's node dies, starting it answers 409. Clone it instead: the clone starts from the last sync point.

**Packages.** Nix packages pinned as `name@version` (`latest`, `N`, `N.N`, `N.N.N`). `packages_list` / `packages_add` / `packages_remove`; `packages_update` re-resolves to newer versions. A version that is not cached answers 422 with the versions that are.

**Builders.** Each owner has a hidden builder that starts on demand for `container_build`. `builder_status` shows it.

**Quota and regions.** `quota` shows what the owner may still allocate (workspaces, environments, snapshots, disk, cpu, memory). Over quota answers 409. `regions` lists where workspaces can run. When a limit or a missing access blocks the person, `request_create` asks a superadmin (kinds quota, access, region, other; one pending per kind); `requests_list` / `request_get` show the decision. Never retry the blocked call hoping it passes.

**Lifecycle calls are asynchronous.** Create, start, stop, clone and restore return at once. Poll `workspace_get` / `env_get` until the state you want. A workspace goes `creating` then `ready` (never `running`); an environment goes `creating` then `running`. Both can also be `stopped`, `error` or `deleted`; stop polling on `error` or `deleted`.

## Tools

Each role has a different set of tools; your role skill lists yours, with when to use each. Not every tool named above is yours: if a tool is not in your list, it is another role's job.

**Every role** has `web_fetch` (read one URL) and `web_search` (search the web).

**Permission cards.** Some calls stop and show the person a card to approve or deny:
- always: `workspace_stop` (except a workspace session stopping its own workspace), `workspace_delete`, `worktree_drop`, `env_stop`, `env_delete`, `env_restore_in_place`, `service_update`, `service_remove`, `volume_delete`, `snapshot_delete`, `packages_remove`, `intercept`, `container_build`, `container_push`;
- unless the sandbox and network fence hold: `exec`, `bash`, `web_fetch`.

These calls take a `because` field. Put the person's exact words in `asked` only when this turn they literally asked for this action and named its target; then the card is skipped. Otherwise write one sentence in `reason` on why the task needs it. A denied card is the person's answer: do not retry the call or reach the same result another way.

## Conventions

These are the team's rules. Follow them.

**Destructive verbs** (delete, stop, restore in place): list, then call. First list the exact targets by name, then call the tool with their ids. The permission prompt is the confirmation; do not ask again in prose.

**No backdoors.** Never reach into another workspace, pod or session to get work done. When a delegation or push fails, report the failure to the person as it is; never route around it.

**Push a snapshot only when asked.** Sync points already cover crash safety.

**Environments.** Intercept the shared team environment from your workspace rather than making a private copy. Release when you are done.

**Packages and setup.** Every workspace session, at start, reads the repo's `AGENTS.md` (its Setup section) and installs the packages it names that are missing. When you install a new package, add it to `AGENTS.md`. Setup requirements live in the repo they belong to.

**Service images.** You decide: `container_build`/`container_push` for code you own, any image for off-the-shelf software.
