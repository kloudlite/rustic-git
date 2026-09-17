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
2. **Work for a workspace is queued into that workspace's session, and done there** (owner,
   05:25 IST: "it should send message to workspace in the queue and it need to be processed
   there"). No `kl_ws_*` direct tool-server tools. One tool, `kl_workspace_ask { workspace,
   request }`: the extension calls the bench's own server (`POST /workspaces/{id}/ask
   {text, from}` on `127.0.0.1:7789`; `from` is the asking session's id, handed to the child at
   spawn as `KL_SESSION`). The bench opens the workspace thread (`openWorkspace`), records an
   exchange (`dir: "out"`, state `queued`), sends the text as a prompt to that child (`followUp`
   when it is mid-turn), transitions the exchange to `running` on its `agent_start` and `done` /
   `failed` on `agent_end`, and delivers that turn's final assistant text back into the asking
   session as a follow-up prompt prefixed `[from workspace <name>]`. The tool answers at once:
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
