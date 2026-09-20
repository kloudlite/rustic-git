# A bench is a Workspace

Owner ask (2026-09-16 21:20 IST): "I also want you to treat bench as a workspace." Owner chose,
from three shapes offered, "A bench IS a Workspace: retire the Bench CRD"; and approved two rulings
in chat: a bench keeps its idle sleep, and its session data moves into the bench's own volume.

Supersedes, for the object model only: `2026-09-13-bench-platform` (decisions 1, 2, 3, 5, 6 are
restated below where they change), `2026-09-13-bench-sessions-server-side-design.md` §folder,
`2026-09-14-bench-tool-credential-design.md` §bench admission, `2026-09-15-team-removal-and-pause`
§Bench access. Everything those specs say about *sessions*, *tool tokens*, *pause/removal
semantics* and *gateway tickets* stays true; only "there is a Bench object" changes.

The code map this was written from: `scratchpad/bench-unify-map.md` (copied into
`docs/superpowers/specs/2026-09-16-bench-is-a-workspace-map.md` for the record).

## What a bench is after this

A **Workspace** with `spec.bench: Some(BenchOptions)`, one per (person, team), named
`bench_id(owner, team)` exactly as today, created on demand by `POST /v1/bench` (or the first
`/v1/bench/session`), in the pair's namespace `ws_namespace(owner, team)`. It has everything a
workspace has: a btrfs `Volume`, push/restore/clone/history, replication, Nix packages and
`spec.locks`, the `user-key` mount, the tool server on 7788, sshd on 22, `kl` with `pkg`/`env`/
`container`, the `workspace-token`. It additionally runs `harness-bench` on 7789 in a second
container of the same pod, with its data under the volume.

```rust
// crates/workspaces/src/crd/workspace.rs
pub struct WorkspaceSpec {
    …existing fields…,
    /// Some = this workspace is the owner's bench in `team`. Written only by `/v1`; the
    /// admission policy refuses any other writer, like every spec field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bench: Option<BenchOptions>,
    /// Membership pause, for EVERY workspace (was `Bench.spec.access`). The membership beat
    /// writes it; the gateway and `/v1` read it. Default Full.
    #[serde(default)]
    pub access: Access,            // Full | Paused  (alias "readOnly" kept for stored objects)
}
pub struct BenchOptions {
    #[serde(default)] pub model: String,
    /// RFC 3339. Written by `/v1` on wake; the idle protocol below.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub wake_at: Option<String>,
}
pub struct WorkspaceStatus { …existing…, #[serde(default)] pub idle_since: Option<String> }
```

`spec.name` = `"bench"`. `spec.region` = the team's bound region (a bench now stores its region
like every workspace; the "read it from the directory per call" rule of bench-platform decision 1
retires, because a Volume pins a workspace to a region anyway). `spec.storage` = the default
workspace storage (quota from `default_quota`'s per-workspace disk). `spec.image` = the
**workspace** image; the bench container's image comes from `KLOUDLITE_BENCH_IMAGE` and is stamped
on the pod by the agent from `ClusterSettings`/env, so a bench follows the configured bench image
on every wake without a spec field (the wake_patch image dance retires).

`is_bench(&Workspace) -> bool` = `spec.bench.is_some()`. It is THE predicate; no code may infer
"bench" from the name prefix, the label or the container list.

## The pod

`workspace_pod(...)` gains a `bench` container when `is_bench`:

- container `bench`: image = configured bench image; command `["harness-bench", "--dir",
  "{workspace_dir}/.bench", "--idle-secs", …]`; env as `k8s/bench.rs` has today (`KL_BENCH`,
  `KL_MODEL`, `KL_BENCH_IDLE_SECS`, `KL_TOOL_TOKEN_FILE`, `KL_API_URL`, `KL_TEAM`, `KL_OWNER`,
  OTLP, `NODE_NAME`) plus `KL_WORKSPACE_ID`, `KL_WORKSPACE`; mounts: the live worktree volume at
  the same path as the workspace container (so `.bench` is inside the btrfs subvolume), home,
  `user-key`, `bench-tool` Secret, tmp; readiness `harness-bench --ping`; `securityContext`
  hardened (no SYS_CHROOT).
- container `workspace`: unchanged (prelude, sshd pid 1, `kl ide serve`).
- Pod `restartPolicy` stays `Always` for the workspace; the bench container's own lifecycle is
  read from `status.containerStatuses[name=bench]`: **the exit-code channel moves from the pod to
  the container.** With `Always`, kubelet would restart an exited bench container; that is what
  we want for a CRASH but not for idle (exit 0) or locked (exit 75). Therefore `harness-bench`
  no longer exits on idle: on idle it writes `{dir}/.idle` (RFC 3339) and **keeps serving**; on
  a locked folder it writes `{dir}/.lock.holder` (exists today) and exits 75 as now (kubelet
  restarts it with backoff, which is the right shape for "someone else holds it").
- The agent's `bench_state` reads idleness through the readiness probe instead of the exit code:
  `harness-bench --ping` returns **non-zero once `.idle` is older than 0 s and no client is
  connected** (`GET /healthz` answers `{"ok":true,"idle":"<ts>"}` and `--ping` exits 2 on idle).
  A bench pod whose `bench` container has `ready=false` with `lastState`/message `idle` for two
  probe periods is the `Idle` verdict → the agent deletes the pod (as today) and stamps
  `status.idleSince`, phase `Idle`. Locked = container `terminated.exitCode == 75` → phase
  `Starting`, reason `FolderLocked`, message names the holder from the termination message (the
  bench writes it there as today).
- Wake: `/v1/bench/session` on an `Idle` bench patches `spec.bench.wake_at` (and
  `desiredState: Running`); `wants_pod(&Workspace)` for a bench = Running && access != Paused &&
  (idle_since none || wake_at > idle_since) — `bench_wants_pod` moved, same truth table. An
  ordinary workspace's `wants_pod` is `desiredState == Running` as today.
- Labels: the pod carries `KIND_LABEL=bench` when `is_bench` (else `workspace`), plus
  `WORKSPACE_LABEL=id`, `OWNER_LABEL`, `TEAM_LABEL`. `allow_bench_tools` keeps its selector
  (target: workspaces that are not benches; from: kind=bench). The bench pod's own tool server on
  7788 is reached by `harness-bench` over `127.0.0.1` (same pod, no policy applies), which is how
  `/pty?scope=bench` now works: **it splices to `127.0.0.1:7788/stream/pty`** — the bench shell IS
  the workspace container's zsh with the Nix profile and the person's packages. `node-pty`, zsh
  and starship leave the bench image again (Task 3 of the kl plan is reverted for the image; the
  shared `shell_rc` constants stay, used by the workspace prelude only).
- `bench_ingress_policy` (gateway → 7789) becomes part of `allow_gateway_ssh`'s sibling for the
  bench pod: one policy `allow-gateway-bench` on `KIND_LABEL=bench`, TCP 7789, same peer.

## Data and migration

- Bench data lives at `{workspace_dir}/.bench/` inside the volume: `sessions/`, `workspaces/`,
  `btw/`, `.lock`, `.lock.holder`, `.health`, `.idle`. It is snapshotted, replicated and pushed
  with the workspace. Git ignores it through the global gitignore (`.bench/` joins `.cache/`,
  `graft/`, `.direnv/` in `deploy/workspace-image/gitignore-global`).
- Legacy folders `{pool}/homes/.benches/{team}/{owner}` are migrated ONCE, by the agent, when it
  first reconciles a bench-flagged Workspace whose volume has no `.bench/` yet and the legacy
  folder exists: `cp -a` into the volume, then rename the legacy folder to
  `{pool}/homes/.benches/{team}/{owner}.migrated-<unix>` (never deleted by this plan; a later
  cleanup is the owner's call). Logged `bench.folder.migrated {bytes, files}`. The flock file is
  not copied (a lock belongs to a process, not to data).
- Existing `Bench` CRs: a one-shot **api admin-boot backfill** (`bench.backfill`, like the
  region backfill) creates the bench Workspace for every live `Bench` (same name, same owner/team,
  `desiredState` copied, `access` copied, `model` copied, region from the directory), then patches
  the legacy Bench to `desiredState: Stopped` so its pod goes away and the flock is released
  before the new pod takes the folder. The legacy `BENCH_FOLDER_FINALIZER` is REMOVED from every
  legacy Bench by the backfill (the folder is migrated, not deleted), and the legacy Bench is
  deleted by the backfill after the Workspace reports `Ready` once. The `Bench` CRD itself is
  deleted from `deploy/k3s/crds.yaml` in a LATER release, after a week with zero legacy objects
  (owner-gated step; see plan).
- `bench_id` names collide with nothing: workspace names are `ws-*`; a test pins that no
  generator yields a `bench-` prefix.

## API

- `/v1/bench*` routes stay, byte-for-byte compatible for the shipped desktop and `kl-connect`
  (same statuses, same `{state}` / `{id, token, gateway, expires_at}` bodies, same `"bench is
  stopped; start it"` sentence). Internally they operate on the bench Workspace: `my_bench` =
  `Workspace::get_opt(bench_id)` filtered `is_bench && owner == caller`. `POST /v1/bench` creates
  the Workspace through the SAME code path as `POST /v1/workspaces` (`create_ws` with
  `bench: Some(..)`, `name: "bench"`, default storage), so quota, key install, region binding
  and audit are one path. `bench_doc` keeps its shape and adds nothing.
- `GET /v1/workspaces` **excludes** `is_bench` rows. `GET /v1/workspaces/{id}` on a bench id
  answers as for any own workspace (the desktop needs it for `kl pkg` inside the bench and for
  the bench's packages page). Every write verb on `/v1/workspaces/{id}` is allowed on a bench
  EXCEPT `DELETE` (409 "a bench is deleted with your membership, not by hand") and `stop` (use
  `/v1/bench/stop`; the two are the same handler after the facade).
- Quota: a bench charges cpu+memory while `wants_pod && phase != Idle` and DISK always (it has a
  volume now — this is the one change to decision 3), never the `workspaces` count and never a
  team. `quota::usage` filters `is_bench` out of the count and into the disk sum.
- `bench_admits_tool(&Workspace, sub, team)` = `is_bench && owner == sub && team == team &&
  desiredState == Running && access == Full`. `WORKSPACE_TOOL_ROUTES` and the workspace token
  work inside the bench unchanged (`KL_WORKSPACE_ID` = the bench id).
- `access` is written by the membership beat on EVERY workspace of the pair (bench and ordinary)
  — `pause()` patches `{"spec":{"access":"paused","desiredState":"stopped"}}` on all of them; the
  gateway's `resolve()` reads `spec.access` off the Workspace it resolves and drops the pair-Bench
  lookup. Removals mark Workspaces only (the bench is one). GC deletes bench workspaces first
  (order preserved: bench → other workspaces → space choice), through the ordinary workspace
  finalizers (worktree + volume), so the bench data goes with the volume like any workspace's.
- Keys: `project_all`, `prune_namespaces`, `space_of` read Workspaces only; the Bench branches go.
- History: `watch.rs` mappers skip `is_bench` objects; no `workspace.*` row is written for a bench
  (the "no admin surface reads a bench" invariant holds — a bench is private to its person).
  Admin RBAC unchanged (it can list Workspaces already; the reflector filter, not RBAC, is the
  fence, as it is for the web). Superadmin workspace lists filter `is_bench` in the api's admin
  handlers too (`admin/owners.rs`, overview counts).

## Gateway

`Ticket::Bench` and `Ticket::Ssh` both stay (they pick the port); both resolve a Workspace now.
`resolve_bench(client, id, 7789)` = `Workspace::get` → `is_bench` else 404 → 403 Paused → 409 not
Ready → podRef → IP. `resolve(...)` for ssh reads `spec.access` directly.

## harness-bench

- `--dir` defaults to `$KL_WORKSPACE/.bench` when `KL_WORKSPACE` is set, else `/bench`.
- Idle: writes `{dir}/.idle` and keeps serving; clears it on the next client; `/healthz` carries
  `idle`; `--ping` exits 2 when idle. Nothing else exits 0 any more except SIGTERM.
- `/pty?scope=bench` splices to `127.0.0.1:7788/stream/pty` (the pod's own tool server). The
  node-pty path is deleted. `spliceWorkspaceShell` is reused with a fixed address.
- Workspace-tools resolve for OTHER workspaces is unchanged (`/v1/workspaces/{id}/tools`).

## Desktop and web

No protocol change and NO listing change: the owner ruled (2026-09-16 23:30 IST) that the bench is
never shown as a workspace in the desktop or the web. It keeps its own place (the bench shell scope,
the sessions view); `GET /v1/workspaces` excludes it, and the desktop does not fetch it as a
workspace. `kl pkg`/`kl env` inside the bench shell are how its packages and environment are managed.

## SLO

All `bench.*` ids keep their meaning through the facade. New: `bench.push.p95` (hourly, group 3:
`POST /v1/workspaces/{bench}/push` completes, `history` lists the snapshot; `95 % ≤ 60000 ms`),
`bench.pkg.add` (hourly, group 3: a package added through the API lands in `spec.packages`;
`99.9 % ≤ 20000 ms`), `bench.migrated` (weekly drill: a legacy folder seeded beside a fresh bench is
copied in on first start and renamed `.migrated-*`; `99.9 %`). `bench.idle.wake` asserts the new
idle signal (`.idle` + readiness false → pod gone → wake).

## Out of scope

Deleting the `Bench` CRD (later release, owner-gated). Deleting `.migrated-*` folders. Multiple
benches per team. Sharing a bench.

## Security summary

No new credential. The bench pod now also has sshd and the tool server, fenced exactly as a
workspace pod is (gateway-only 22, namespace-only 7788), plus gateway-only 7789. `access` on
every Workspace makes the pause check one read instead of a cross-object lookup. Bench data is
now in a replicated volume: a person's transcripts survive a node loss (they did not before).
