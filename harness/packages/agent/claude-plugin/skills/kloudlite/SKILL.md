---
name: kloudlite
description: You run on Kloudlite; "workspace", "environment", "service", "snapshot" in a request mean Kloudlite's, driven by the workspace_*/env_*/service_*/intercept/packages_* tools ("delete all workspaces" = list them with workspace_list, then workspace_delete each). Read this before any task that creates, changes, stops, deletes, lists or works inside a workspace or environment, or mentions intercept, push, restore, clone, package or build: concepts, which tool for what, and the team's conventions.
---

# Kloudlite

Kloudlite gives each person cloud dev machines (workspaces) and shared running stacks (environments) on one cluster. You drive it through the platform tools. "Delete all workspaces" means Kloudlite workspaces: list them with `workspace_list`, then delete them, following the convention for destructive verbs below.

## Where you are: three kinds of session

Find your row first. Your tool list tells you which one you are: main has `workspace_ask`, a workspace session has `subagent`, a subagent has neither.

| Session | Key | Lives in | Tools | Talks to |
|---|---|---|---|---|
| **Main** | `main` | the bench; no workspace, no source code | every platform tool, `workspace_ask`, and `bash`/`read`/`write` confined to a scratch folder (`/tmp/kl-main/<session>`, gone on bench restart) | the person; workspaces through `workspace_ask` |
| **Workspace** | `<ws>` | that workspace's own pod, `~/workspace` | the pod's code tools (read, write, edit, exec, grep, ...), the platform tools for its own workspace, and `subagent` | whoever asked it (the person or main), through its answer; its subagents through `subagent` |
| **Subagent** | `<ws>:agent-<hex>` | its own throwaway clone of one workspace, `~/workspace` | the same tools as a workspace session, for its own clone; no `subagent`, no `workspace_ask` | only the workspace session that started it, through its final answer |

A workspace or subagent session's working directory is `~/workspace` in its pod: relative paths resolve there and projects go under it.

Every session is isolated: it works only in its own folder and pod. No session reads, runs or changes code in another workspace, pod or session. The only ways work crosses between sessions are the two tools below and their answers.

## Workspaces and subagents: which one, and how they talk

**What each is for.**
- A **workspace** is a long-lived home for one component (frontend, backend, worker, test suite). Its session owns that component's design and code, runs its service, holds its intercept, and keeps its working branch. Make a new workspace for a new component, not for a task.
- A **subagent** is a throwaway worker for one task inside one workspace. It gets its own clone of that workspace, does the task, its commits are pushed into the workspace's working branch, and the clone is deleted. It never outlives its task.

**Who uses which.**

| You are | The work is | Do |
|---|---|---|
| main | anything that touches code, packages or a running service in a workspace | `workspace_ask` that workspace with the goal. Never do it yourself, not even "just a small fix". |
| main | work spanning several workspaces | `workspace_ask` each workspace its own part, in the order the parts depend on each other (below) |
| main | platform-level only: create, list, stop, delete, clone, intercept, environments, quota | the platform tool, yourself |
| workspace | a small edit, running or restarting the service, reading logs, answering a question about the code | do it yourself |
| workspace | planned work: a feature, a refactor, a multi-step fix, anything that needs a plan and execution | `subagent` with the task |
| subagent | your task | do it in your clone; you cannot hand it on |

**Main and a workspace: `workspace_ask`.**
1. Main calls `workspace_ask { workspace, request }`. The request is the person's goal in their words, plus context only main has: which environment, what the person decided, facts from another workspace's answer. Never file paths, languages, libraries, layout, endpoints or steps.
2. The call returns at once. The workspace session receives `[from main session] <request>` as a new turn (queued after its current turn if it is busy).
3. The workspace's answer is the last text of its turn. It arrives in main later as a message starting `[from <ws>] ...` (or `[from <ws>] failed: ...`). Do not wait, sleep or poll for it; keep serving the person and act when it arrives.
4. An ask in flight survives a bench restart: it is resent (marked `[resent after restart]`) up to twice, then main gets `failed: lost in 3 bench restarts`.
5. A workspace session's final text IS its report to main. End every asked turn with what was done, what changed (branch, commit, files), and anything main or the person must decide. A workspace cannot message main mid-task, and cannot ask another workspace anything.

**Work across workspaces.** Workspaces never talk to each other. Main is the only bridge: ask the workspace that provides something first (the backend for an API), take the facts the frontend needs from its answer (the endpoint and payload it reports), and pass those as context in the next ask. Independent parts can be asked at the same time.

**A workspace and its subagent: `subagent`.**
1. Commit your own work first. The clone copies your folder as it is, and the subagent's commits are pushed into your checked-out branch; uncommitted changes of yours get in the way.
2. Call `subagent { task }`. The subagent starts with no memory of your conversation: the task must stand alone (goal, constraints, what done looks like, how to check it). You may say how to build it; it is your component.
3. The call blocks until the subagent finishes. Clones of one workspace are cut one at a time, so a second `subagent` waits for the first clone to be cut.
4. The subagent works and commits in its clone's `~/workspace` on your branch. It does not push and does not touch your workspace: when it ends, the platform commits whatever it left, pushes it into your branch with git, and deletes the clone.
5. If your branch moved meanwhile, the platform asks the subagent once to `git pull --rebase` and resolve the conflicts in its clone, then pushes again.
6. You get its final answer plus one of: `pushed <sha> to <branch> in <ws>` and the changed files; `no code changes`; or `push failed: ...; clone <id> kept with the commits`. Report a failure as it is. Never fetch from the clone, copy files across or work around it.
7. After a landed task, push your working branch to the origin repo with `git push` through `exec`.

**Permission cards** from a workspace or subagent session go to whoever is watching the session that asked, filed under the asking session's name.

## Concepts

**Workspace.** One dev machine: a pod with its own home (`/home/kl`, a btrfs volume), source under `~/workspace`, and its own installed packages. Usually one workspace per component: frontend, backend, worker, test suite. Components can come from different repos or the same one. Each kind tends to need different packages.

**Worktree.** An extra working copy inside a workspace (`worktree_add` / `worktree_drop`), for a second branch without a second workspace.

**Environment.** A shared, multi-service stack for a team (the backend, its database, a queue...). Each service runs as its own StatefulSet. Add, change and remove services with `service_add` / `service_update` / `service_remove`.

**Service image.** When you add a service, you choose the image: build one with the `container_build` tool (`tags: ["name:tag"]`, context in your working directory; it builds on your builder and pushes to the Kloudlite registry) and retag with `container_push`. `kl` is not on PATH inside `exec`; these tools run it for you, or use any public image.

**Intercept.** Route one service of an environment to a workspace instead of the service's own pod. Traffic for that service then reaches the code running in your workspace. `intercept` starts it, `release` ends it. Only one workspace can intercept a service at a time. If the workspace stops, the intercept is released after a short grace period, but the wish stays until `release`.

**Space.** `space_env_current` / `space_env_switch` / `space_env_clear`. A team's space follows one environment, so its workspaces can reach that environment's services by DNS name.

**Snapshots.** A read-only copy of a workspace's or environment's disk.
- *Sync points* are cut automatically while a workspace runs. They are for crash safety only and are never shown as history.
- *Push* (`workspace_push` / `env_push`) records a named snapshot in history. Pushes are never pruned.
- *Restore* (`workspace_restore`, `env_restore`) makes a new copy from a snapshot. `env_restore_in_place` rewrites the environment itself.
- *Clone* (`workspace_clone`, `env_clone`) copies a workspace or environment. The reply's `based_on` says how old the copy is.
- `volume_list` / `volume_history` / `volume_refs` read the chain; `snapshot_delete` and `volume_delete` remove history.

**Interrupted workspace.** If a workspace's node dies, starting it answers 409. Clone it instead: the clone starts from the last sync point.

**Packages.** Nix packages pinned as `name@version` (`latest`, `N`, `N.N`, `N.N.N`). `packages_list` / `packages_add` / `packages_remove`; `packages_update` re-resolves to newer versions. A version that is not cached answers 422 with the versions that are.

**Builders.** Each owner has a hidden builder that starts on demand for `container_build`. `builder_status` shows it.

**Quota and regions.** `quota` shows what the owner may still allocate (workspaces, environments, snapshots, disk, cpu, memory). Over quota answers 409. `regions` lists where workspaces can run. When a limit or a missing access blocks the person, `request_create` asks a superadmin (kinds quota, access, region, other; one pending per kind); `requests_list` / `request_get` show the decision. Never retry the blocked call hoping it passes.

**Lifecycle calls are asynchronous.** Create, start, stop, clone and restore return at once. Poll `workspace_get` / `env_get` until the state you want.

## When to use what

| You want to | Use |
|---|---|
| a small edit, run the service, check its logs | the workspace's own session (`workspace_ask` from main) |
| planned work: a feature, a refactor, a multi-step fix | `workspace_ask` the workspace; its session runs a `subagent` |
| a new component | `workspace_create` one workspace for it |
| test your change against the team's real stack | `intercept` the service in the team environment, `release` when done |
| a second branch in the same workspace | `worktree_add` |
| a copy to experiment on | `workspace_clone` |
| keep a named point in history | `workspace_push` (only when asked) |
| go back to a pushed point | `workspace_restore` |
| a new tool installed | `packages_add`, then update AGENTS.md |
| a new service in the environment | `service_add`, image from `container_build`/`container_push` or any image |
| see what is running | `workspace_list`, `env_list`, `workspace_get`, `env_get` |
| see what is left | `quota` |
| ask for more quota, access or a region | request_create |
| see what was asked and decided | requests_list |

## Conventions

These are the team's rules. Follow them.

**Destructive verbs** (delete, stop, restore in place): list, then call. First list the exact targets by name, then call the tool. The permission prompt is the confirmation; do not ask again in prose.

**Main delegates the goal, not the code.** Main passes the person's request as they said it, plus the context it alone has (which workspace, which environment, what the person decided). It never picks the language, file paths, layout, endpoints or libraries, and never writes steps. The workspace session decides how.

**No backdoors.** Every session works only in its own working folder. A session never reaches into another workspace, pod or session to get work done. When a delegation or push fails, report the failure to the person as it is; never route around it.

**Push a snapshot only when asked.** Sync points already cover crash safety.

**Who does what.** In the owner's words: "main workspace is used for small works, running the service and it will be the one usually intercepting and it will be maintaining working branch. other subagents will have to push and resolve conflicts here and then push to the main repo. any work that need planing and execution it will have to go to agent."
- The workspace's own session does small edits, runs the service, holds the intercept, and owns the working branch.
- Anything that needs planning and execution goes to a subagent. The subagent works in its own clone; the platform pushes its commits into the workspace's working branch, and the subagent resolves any conflict in its clone before that push.
- The workspace session pushes the working branch to the origin repo after each landed task.

**Environments.** Intercept the shared team environment from your workspace rather than making a private copy. Release when you are done.

**Packages and setup.** Every session, at start, reads the repo's `AGENTS.md` (its Setup section) and installs the packages it names that are missing. When you install a new package, add it to `AGENTS.md`. Setup requirements live in the repo they belong to.

**Service images.** You decide: `container_build`/`container_push` for code you own, any image for off-the-shelf software.
