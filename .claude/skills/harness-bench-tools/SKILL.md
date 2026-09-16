---
name: harness-bench-tools
description: Use when changing what a bench or workspace session can do in the Kloudlite harness — adding, removing or reshaping kl_* tools, the pi spawn arguments, the system prompt, or the bench-to-workspace ask path. Encodes the owner's rulings on who may touch what.
---

# Harness bench tools

The harness runs one agent process (pi, never named to the model) per session inside the person's
bench pod. Three kinds of session, three tool sets. The rulings below are the owner's
(2026-09-17) and are not up for re-litigation in a task.

## The rules

1. **A bench session has no hands in its own pod.** No filesystem, no shell, no process tool.
   It is spawned with `--no-builtin-tools` and loads only `pi/kloudlite.ts`. Anything that would
   run a command in the bench pod is a defect, not a feature request.
2. **A bench session never touches a workspace's resources directly.** No workspace tool-server
   calls from the bench (no `kl_ws_*`, no read/write/exec by workspace id). Every mutation meant
   for a workspace — files, packages, commands, anything — is a MESSAGE queued into that
   workspace's own session through `kl_workspace_ask`. The workspace session does the work with
   its own hands, so its transcript carries the context of what changed and why. If a workspace
   has no session yet, the ask creates one.
3. **A bench session's own machine is the default target.** The bench is itself a workspace
   (`KL_WORKSPACE_ID`). "Install X", "add a package", "switch the environment" with no workspace
   named act on the bench: `kl_pkg_*`, `kl_env_*`. A named workspace is asked, never changed.
4. **The platform is reached only through `kl_*` tools**, each one a `/v1` call listed in
   `pi/catalog.ts` with its effect (read / write / destroy). A person asks for something and no
   tool covers it → add the tool (and, if `/v1` lacks the verb, the route) — never let the model
   improvise with a token. A tool that takes a whole list (services, packages) must be
   read-modify-write by name so an add cannot drop a sibling's fields (the mongodb-mount lesson).
5. **The model does not know what it runs on.** `tellItWhereItStands` REPLACES the system prompt:
   it is "the Kloudlite harness"; no "pi", no `/opt/harness`, no coding-agent boilerplate. The
   btw fork gets the same identity with `--no-tools`.
6. **A workspace session's hands are its own workspace only.** `workspace-tools.ts` maps
   read/write/edit/bash/grep/find/ls to that workspace's tool server; `kloudlite.ts` in workspace
   mode adds only `kl_pkg_*` for that machine. `--tools` is a strict allow-list over extension
   tools too, so a new own-workspace tool must be added to `WORKSPACE_TOOLS` or it cannot be called.

## Where things live

| Concern | File |
|---|---|
| Spawn arguments per session kind, `KL_SESSION`, `KL_TOOLS_WORKSPACE` | `harness/bench/src/rpc-child.ts` |
| Tool catalogue (model description = Settings › Tools row) | `harness/pi/catalog.ts` |
| `kl_*` platform tools, own-machine tools, identity prompt | `harness/pi/kloudlite.ts` |
| Workspace session hands | `harness/pi/workspace-tools.ts` |
| Ask path: `POST /workspaces/{id}/ask`, exchange states, reply delivery | `harness/bench/src/server.ts`, `bench.ts`, `exchanges.ts` |
| Design record | `docs/superpowers/specs/2026-09-17-bench-tools-no-fs-design.md` |

## Checklist for any change

- Every registered tool is in `catalog.ts`; the test that compares them stays green.
- Spawn-arg tests cover bench / workspace / fork.
- The system prompt test still finds no "pi" and no "/opt/harness".
- A new write on a workspace from the bench? Stop: it goes through `kl_workspace_ask`.
- Gates: `cd harness && npm run typecheck && npm run bench:test && npm run build`.
- Probes `bench.tools.no_fs` and `bench.shell.workspace` hold it on the fleet.
