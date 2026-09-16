# Bench sessions: platform tools only, workspaces through their tool servers

Owner rulings (2026-09-17 05:00–05:10 IST), from one bench transcript: asked "add nats to the
env", the model had no tool for it, so it read `/opt/harness/pi/kloudlite.ts`, cat'd the tool
token into ad-hoc node scripts against `/v1`, then created a second `devstack` and deleted the
first. Rulings: "you need to update all tools in the pi session"; "I don't like the fact it is
going and reading kloudlite.ts"; "it should be deprived of fs tools"; "actually it should be
deprived of exec tool too. it should use workspace ide tools directly".

## Design

1. **A bench session has no hands in the bench pod.** pi is spawned with `--no-builtin-tools`;
   `background.ts` and `process.ts` (both bench-pod `bash`) are not loaded for a bench session.
   The `btw` fork runs with `--no-tools`. Workspace sessions are unchanged (`--tools` allow-list
   on their own workspace).
2. **Workspaces are reached through their tool servers, by id.** A new extension
   `workspaces.ts` registers `kl_ws_read`, `kl_ws_write`, `kl_ws_edit`, `kl_ws_exec`,
   `kl_ws_grep`, `kl_ws_glob`, `kl_ws_ls`, each taking `workspace` (id) plus the same parameters
   `workspace-tools.ts` maps today (`toIde`/`fromIde` are reused, not copied). The address comes
   from `GET /v1/workspaces/{id}/tools?team=` (cached per id, re-asked on a connection error,
   exactly as `workspace-tools.ts` does). A workspace that is stopped answers with the tool
   server's refusal, never a start: starting is `kl_workspace_start`, the person's call.
3. **The catalogue covers `/v1`.** New tools (all through `kloudlite.ts`'s `reg`, named in
   `catalog.ts` with effect): `kl_environment_services` (PATCH `/v1/environments/{id}` `{services}`,
   effect write), `kl_workspace_packages_update` (POST `/v1/workspaces/{id}/packages/update`),
   `kl_workspace_restore` (POST `/v1/workspaces/restore`), `kl_environment_restore` (POST
   `/v1/environments/restore`), `kl_environment_restore_in_place`, `kl_volume_history` (GET
   `/v1/volumes/{name}/history`), `kl_volume_delete` (DELETE, effect delete), `kl_requests` /
   `kl_request_create` (`/v1/requests`). The exchange regex in `kloudlite.ts` also matches `kl_ws_`.
4. **`PATCH /v1/environments/{id}` `{services}`** is new on the api: `check_services`,
   `guard_alloc` for the ADDED service count only (removals free), merge-patch `spec.services`;
   refused 409 while an intercept names a service being removed. The controller prunes: a
   StatefulSet or ClusterIP Service in the env namespace whose name is not in `spec.services`
   is deleted on the next reconcile (owned by the Environment, labelled as today). Mount folders
   are never deleted — bytes stay on the volume until the volume goes. `env_doc` unchanged.
5. **The model is told where it stands.** `kloudlite.ts` adds a system-prompt line via pi's
   extension hook: it runs on a bench with no filesystem or shell of its own; workspaces are
   acted on through `kl_ws_*`; the platform only through `kl_*`; it never probes the platform
   another way. **It does not know it is pi** (owner, 05:15 IST): the extension REPLACES pi's
   default system prompt rather than appending — no "pi", no coding-agent boilerplate, no paths
   under `/opt/harness`; it reads and acts as "the Kloudlite harness" (owner, 05:17 IST) — the person's bench on the platform — and its tools are the whole world it sees.
6. **Probe.** Hourly `env.services.patched`: add a service to the run's environment via the
   PATCH, both ready; remove it, its StatefulSet gone within the stage budget. `bench.tools.no_fs`
   (hourly): the bench session's tool list has no `bash`/`read`/`write` and has `kl_ws_read`.

## Out of scope
Renaming a service in place (remove + add). Per-tool approval in the desktop. Bench `bash` for
power users (owner ruled against).
