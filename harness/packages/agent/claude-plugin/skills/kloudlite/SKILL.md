---
name: kloudlite
description: You run on Kloudlite; "workspace", "environment", "service", "snapshot" in a request mean Kloudlite's, driven by the workspace_*/env_*/service_*/intercept/packages_* tools ("delete all workspaces" = list them with workspace_list, then workspace_delete each). Read this before any task that creates, changes, stops, deletes, lists or works inside a workspace or environment, or mentions intercept, push, restore, clone, package or build: concepts, which tool for what, and the team's conventions.
---

# Kloudlite

Kloudlite gives each person cloud dev machines (workspaces) and shared running stacks (environments) on one cluster. You drive it through the platform tools. "Delete all workspaces" means Kloudlite workspaces: list them with `workspace_list`, then delete them, following the convention for destructive verbs below.

## Where you are: three kinds of session

| Session | Runs | Tools |
|---|---|---|
| **Main** (`main`) | the bench, no workspace of its own | every platform tool, `workspace_ask`, `subagent`, and `bash`/`read`/`write` confined to a scratch folder (`/tmp/kl-main/<session>`, gone on bench restart) |
| **Workspace** (`<ws>`) | that workspace's own pod | the pod's code tools (read, write, edit, exec, grep, ...) plus the platform tools for its own workspace; its workspace is fixed and its environment is the default |
| **Subagent** (`<ws>:agent-<hex>`) | its own clone of a workspace | code tools only |

A workspace or subagent session's working directory is `~/workspace` in that workspace's pod: relative paths resolve there and projects go under it.

Main has no source code. To change code or run something in a workspace, go through that workspace's session (`workspace_ask`) or a subagent.

## Concepts

**Workspace.** One dev machine: a pod with its own home (`/home/kl`, a btrfs volume), source under `~/workspace`, and its own installed packages. Usually one workspace per component: frontend, backend, worker, test suite. Components can come from different repos or the same one. Each kind tends to need different packages.

**Worktree.** An extra working copy inside a workspace (`worktree_add` / `worktree_drop`), for a second branch without a second workspace.

**Environment.** A shared, multi-service stack for a team (the backend, its database, a queue...). Each service runs as its own StatefulSet. Add, change and remove services with `service_add` / `service_update` / `service_remove`.

**Service image.** When you add a service, you choose the image: build one with `kl build -t name:tag .` and `kl push name:tag` (inside a workspace, to the Kloudlite registry), or use any public image.

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

**Builders.** Each owner has a hidden builder that starts on demand for `kl build`. `builder_status` shows it.

**Quota and regions.** `quota` shows what the owner may still allocate (workspaces, environments, snapshots, disk, cpu, memory). Over quota answers 409. `regions` lists where workspaces can run.

**Lifecycle calls are asynchronous.** Create, start, stop, clone and restore return at once. Poll `workspace_get` / `env_get` until the state you want.

## When to use what

| You want to | Use |
|---|---|
| a small edit, run the service, check its logs | the workspace's own session (`workspace_ask` from main) |
| planned work: a feature, a refactor, a multi-step fix | a `subagent` in that workspace |
| test your change against the team's real stack | `intercept` the service in the team environment, `release` when done |
| a second branch in the same workspace | `worktree_add` |
| a copy to experiment on | `workspace_clone` |
| keep a named point in history | `workspace_push` (only when asked) |
| go back to a pushed point | `workspace_restore` |
| a new tool installed | `packages_add`, then update AGENTS.md |
| a new service in the environment | `service_add`, image from `kl build`/`kl push` or any image |
| see what is running | `workspace_list`, `env_list`, `workspace_get`, `env_get` |
| see what is left | `quota` |

## Conventions

These are the team's rules. Follow them.

**Destructive verbs** (delete, stop, restore in place): list, then call. First list the exact targets by name, then call the tool. The permission prompt is the confirmation; do not ask again in prose.

**Main delegates the goal, not the code.** Main passes the person's request as they said it, plus the context it alone has (which workspace, which environment, what the person decided). It never picks the language, file paths, layout, endpoints or libraries, and never writes steps. The workspace session, or a subagent, decides how.

**Push a snapshot only when asked.** Sync points already cover crash safety.

**Who does what.** In the owner's words: "main workspace is used for small works, running the service and it will be the one usually intercepting and it will be maintaining working branch. other subagents will have to push and resolve conflicts here and then push to the main repo. any work that need planing and execution it will have to go to agent."
- The workspace's own session does small edits, runs the service, holds the intercept, and owns the working branch.
- Anything that needs planning and execution goes to a subagent. The subagent works in its own clone, pushes into the workspace's working branch, and resolves conflicts there.
- The workspace session pushes the working branch to the origin repo after each landed task.

**Environments.** Intercept the shared team environment from your workspace rather than making a private copy. Release when you are done.

**Packages and setup.** Every session, at start, reads the repo's `AGENTS.md` (its Setup section) and installs the packages it names that are missing. When you install a new package, add it to `AGENTS.md`. Setup requirements live in the repo they belong to.

**Service images.** You decide: `kl build`/`kl push` for code you own, any image for off-the-shelf software.
