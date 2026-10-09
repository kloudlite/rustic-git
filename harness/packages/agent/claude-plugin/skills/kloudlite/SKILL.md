---
name: kloudlite
description: You run on Kloudlite. In a request, the words "workspace", "environment", "service" and "snapshot" mean Kloudlite workspaces, environments, services and snapshots. You control them with the workspace_*, env_*, service_*, intercept and packages_* tools. Example: "delete all workspaces" means list them with workspace_list, then call workspace_delete for each. Read this skill before a task that creates, changes, stops, deletes or lists a workspace or environment, or that works in a workspace. Also read it before a task about intercept, push, restore, clone, package or build. It gives the concepts, the permission cards and the team rules. Your role (main session or workspace session) has its own skill next to this skill. Read the two skills.
---

# Kloudlite

This skill is written in ASD-STE100 Simplified Technical English. Each word has one meaning. Each instruction is one sentence.

Kloudlite gives each person dev machines in the cloud (workspaces) and shared stacks that run (environments). All of them are on one cluster. You control Kloudlite with the platform tools.

The request "delete all workspaces" means Kloudlite workspaces. Only the main session creates and deletes workspaces. To do this, list the workspaces with `workspace_list`. Then delete them, and obey the rule for destructive actions in "Team rules".

## Two roles

Each session has one role. One skill tells you what your role does. You have this skill and one role skill: `main-session` or `workspace-session`. Your first message also gives the role (`[role: ...]`). Read your role skill first. This skill gives only the shared concepts.

| Role | Where it runs | What it does |
|---|---|---|
| **Main** | On the bench. It has no workspace and no source code. | Talks to the person. Creates, clones, starts and deletes workspaces and environments. Keeps the task board. Gives each task to a workspace with `workspace_ask`. |
| **Workspace** | In the pod of one workspace, in `~/workspace`. | Owns one component: its design, code, service, intercept and working branch. Sends reports to main with `main_tell`. Can stop its own workspace when the task is done. |

Each session is isolated. It works only in its own folder and pod. A session does not read, run or change code in a different workspace, pod or session.

Work moves between sessions only in these three ways:

- `workspace_ask`: main gives work to a workspace. It includes a task id when the work is on the board.
- `main_tell`: a workspace sends main a report (`done`, `blocked` or `need`).
- The answer to an ask.

Workspaces do not send messages to other workspaces. Main moves facts between them.

## Ids and names

Each workspace has a name and an id. The id starts with `ws-`. Each environment has a name and an id. The id starts with `env-`.

1. Get the id from `workspace_list` or `env_list`.
2. Use the id in the `workspace` or `env` parameter of a tool call. Do not use the name.
3. Use the name when you write to the person.

## Concepts

**Workspace.** One dev machine. It is a pod with these items:

- Its own home, `/home/kl`. This is a btrfs volume.
- Source code in `~/workspace`.
- Its own installed packages.

Use one workspace for each component, for example frontend, backend, worker or test suite. Components can come from different repos or from the same repo. Each type of component usually needs different packages.

**Worktree.** An extra working copy in a workspace. It gives a second branch without a second workspace. Use `worktree_add` and `worktree_drop`.

**Environment.** A shared stack of services for a team. Example: the backend, its database and a queue. Each service runs as its own StatefulSet. Use `service_add`, `service_update` and `service_remove` to add, change and remove services.

**Service image.** When you add a service, you select its image. You can use a public image. To build an image, use the `container_build` tool:

- Give `tags: ["name:tag"]`.
- The build context is your working directory.
- The tool builds on your builder and pushes the image to the Kloudlite registry.

To give an image a new tag, use `container_push`. The `kl` command is not on the PATH in `exec`. These tools run `kl` for you.

**Intercept.** An intercept sends the traffic for one service of an environment to a workspace. The service pod does not get this traffic. The code that runs in your workspace gets it.

- `intercept` starts an intercept. `release` ends it.
- Only one workspace at a time can intercept a service.
- If the workspace stops, the platform releases the intercept after a short time. But the request for the intercept stays until `release`.

**Space.** A space follows one environment. Then the workspaces of the team can get to the services of that environment by their DNS names. Use `space_env_current`, `space_env_switch` and `space_env_clear`.

**Snapshots.** A snapshot is a read-only copy of the disk of a workspace or environment.

- *Sync points*: The platform makes them automatically while a workspace runs. They are only for recovery after a crash. They do not show in the history.
- *Push* (`workspace_push`, `env_push`): Records a named snapshot in the history. The platform does not remove pushes. This push is not `git push`.
- *Restore* (`workspace_restore`, `env_restore`): Makes a new copy from a snapshot. `env_restore_in_place` writes the snapshot over the environment.
- *Clone* (`workspace_clone`, `env_clone`): Copies a workspace or environment. The `based_on` field in the reply tells you the age of the copy.
- `volume_list`, `volume_history` and `volume_refs` read the history. `snapshot_delete` and `volume_delete` remove history.

**Interrupted workspace.** If the node of a workspace stops, `workspace_start` answers 409. Then clone the workspace. The clone starts from the last sync point.

**Packages.** Nix packages with a version: `name@version`. The version is `latest`, `N`, `N.N` or `N.N.N`.

- Use `packages_list`, `packages_add` and `packages_remove`.
- `packages_update` finds newer versions.
- If a version is not in the cache, the answer is 422. The answer gives the versions that are in the cache.

**Builders.** Each owner has a hidden builder. It starts when `container_build` needs it. `builder_status` shows it.

**Quota and regions.**

- `quota` shows the resources that the owner can still use: workspaces, environments, snapshots, disk, cpu and memory.
- A call that goes above the quota answers 409.
- `regions` gives the regions where workspaces can run.
- If a limit or missing access stops the person, use `request_create` to ask a superadmin. The kinds are quota, access, region and other. Only one request of each kind can wait at a time.
- `requests_list` and `request_get` show the decision.
- Do not try the stopped call again. It will not pass.

**Lifecycle calls are asynchronous.** Create, start, stop, clone and restore return immediately. The work continues after the return. Poll `workspace_get` or `env_get` until you get the state that you want.

| Object | States | The state when it is up |
|---|---|---|
| Workspace | `creating`, `ready`, `stopped`, `error`, `deleted` | `ready`. A workspace is not `running` at any time. |
| Environment | `creating`, `running`, `stopped`, `error`, `deleted` | `running` |

Stop the poll when the state is `error` or `deleted`. That state will not change to the state that you want.

## Tools

Each role has a different set of tools. Your role skill gives your tools and when to use each tool. This skill gives the names of some tools that are not yours. If a tool is not in your list, it is the work of the other role.

**Each role** has `web_fetch` (read one URL) and `web_search` (search the web).

**Permission cards.** Some calls stop and show a card to the person. The person accepts or refuses the call.

- These calls always show a card: `workspace_stop`, `workspace_delete`, `worktree_drop`, `env_stop`, `env_delete`, `env_restore_in_place`, `service_update`, `service_remove`, `volume_delete`, `snapshot_delete`, `packages_remove`, `intercept`, `container_build`, `container_push`.
- One exception: a workspace session that stops its own workspace does not get a card.
- These calls show a card if the sandbox and the network fence are not in force: `exec`, `bash`, `web_fetch`.

These calls have a `because` field. Fill it in this way:

1. Put the exact words of the person in `asked` only if both conditions are true:
   - In this turn, the person asked for this action.
   - The person gave the target.
   Then the card does not show.
2. In all other conditions, write one sentence in `reason`. Tell why the task needs this call.

If the person refuses a card, that is their answer. Do not try the call again. Do not get the same result in a different way.

## Team rules

These are the rules of the team. Obey them.

**Destructive actions** (delete, stop, restore in place):

1. List the exact targets by name.
2. Call the tool with their ids.

The permission card is the confirmation. Do not ask the person again in text.

**No backdoors.** Do not go into a different workspace, pod or session to do work. If a delegation or a push fails, tell the person about the failure as it is. Do not go around it.

**Push a snapshot only when the person asks.** The sync points already give recovery after a crash.

**Environments.** Intercept the shared team environment from your workspace. Do not make a private copy of the environment. Release the intercept when you are done.

**Packages and setup.** At the start, each workspace session reads the Setup section of `AGENTS.md` in the repo. Then it installs the missing packages that this section gives. When you install a new package, add it to `AGENTS.md`. The setup requirements of a repo are in that repo.

**Service images.** You make the decision. For code that you own, use `container_build` and `container_push`. For standard software, use any image.
