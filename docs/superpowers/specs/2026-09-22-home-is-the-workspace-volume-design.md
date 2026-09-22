# Home is the workspace volume

**Date:** 2026-09-22 · **Owner ruling:** no shared home; each workspace's btrfs volume IS `/home/kl`;
source under `~/workspace`. "This will simplify our design."

## Why

The shared per-region NFS home plus the `/home/kl/workspaces/<name>` tree existed to let other
coding harnesses work against our pods: one home for a person, many project folders inside it.
That goal is gone. What it costs today is three homes with one rule each (NFS config, workspace
`.cache`, node-local `homecache`), an NFS mount gate on every workspace start (`HomeNotReady`),
six `subPath` mounts to make editor servers and the cargo registry travel, an Azure Files share per
region, a `hostPID` DaemonSet whose only reason is that mount, and a bench folder scheme that
piggybacks on all of it. One volume per workspace holding the whole home removes every one of
those. Nothing is shared between workspaces except what the platform mints and mounts itself.

## Layout

A workspace's btrfs subvolume (`Engine::worktree(volume, ws)`, unchanged path) is mounted at
`/home/kl`. Inside it:

| path | what | who writes |
|---|---|---|
| `~/workspace/` | the source tree; `repo`/`branch` seeding clones here; `kl` and the tool server root here | person |
| `~/.cache/` | every cache: XDG default, cargo target, go, npm, pnpm, bun, yarn, pip, uv, gomod, m2, composer, nuget, rustup, playwright, editor remote servers | tools |
| `~/.cargo/`, `~/.config/`, `~/.gradle/`, `~/.local/`, shell rc, history | config and state | person and tools |
| `~/.kl-home` | layout marker, empty file, uid 1000; its presence means "this tree is a home" | agent |

Everything in the tree is snapshotted, pushed, replicated, cloned and restored as one unit. There
is no travels/stays split and no per-workspace exclusion list: **nothing the platform mints lives in
the volume.** The things that must not travel are already mounts over the tree, and stay so:

- `/home/kl/.ssh/authorized_keys` — hostPath `type: File` from `{pool}/keys/{owner}/authorized_keys` (unchanged).
- `/etc/resolv.conf` — hostPath file from `{pool}/attach/{ws}/resolv.conf` (unchanged).
- the `user-key` Secret at `USER_KEY_PATH` (unchanged).
- `/tmp` — an `emptyDir`; `TMPDIR` is unset (tools default to `/tmp`).
- `/nix/store` and the profile — read-only hostPath (unchanged).

A person's own credentials (`~/.cargo/credentials.toml`, `~/.config/gh`, …) travel with clone and
restore. That is correct: a clone or a restore is always the same owner's workspace (`/v1` places
it under the source's `spec.owner`), so nothing crosses a trust boundary. The docs say so in one
line under limits.

`login_env` shrinks to the variables whose default is NOT under `~/.cache`: `CARGO_TARGET_DIR`
(`~/.cache/cargo-target`, so a repo's `./target` never collides with versioned files),
`RUSTUP_HOME` (`~/.cache/rustup`), `GOMODCACHE` (`~/.cache/gomod`), `MAVEN_OPTS`
(`-Dmaven.repo.local=~/.cache/m2`), `PLAYWRIGHT_BROWSERS_PATH` (`~/.cache/ms-playwright`),
`NUGET_PACKAGES` (`~/.cache/nuget`), `DO_NOT_TRACK`. `XDG_CACHE_HOME`, `GOCACHE`,
`npm_config_cache`, `PNPM_STORE_DIR`, `BUN_INSTALL_CACHE_DIR`, `YARN_CACHE_FOLDER`,
`COMPOSER_CACHE_DIR`, `UV_CACHE_DIR`, `PIP_CACHE_DIR`, `GRADLE_USER_HOME`, `TMPDIR`, `HISTFILE`
go: each tool's default already lands in `~/.cache` or `~` and both now travel. The six `live`
subPath mounts (`~/.cargo/registry`, `.vscode-server`, `.cursor-server`, `.zed_server`,
`.windsurf-server`, `.jetbrains`) go: they are ordinary directories in the home now. `KL_WORKSPACE`
and the prelude's `cd` into it stay, pointing at `/home/kl/workspace`. The global gitignore is
unchanged (`.cache/`, `graft/`, `.direnv/`).

`k8s::WORKSPACE_DIR = "/home/kl/workspace"` replaces `WORKSPACES_DIR` and `workspace_dir(name)`;
`HOME_CACHE_DIR`, `HOME_STATE_DIR`, `SEED_DIR` go. The pod's `workspaces` emptyDir and the per-name
bind mount go; the `live` hostPath mounts at `HOME_DIR` with `mountPropagation` unset.

## What goes

| today | after |
|---|---|
| `ensure_shared_home`, `HomeNotReady` gate and stats reason, `ctx_with(out)_homes_export` test helpers | deleted now; `apply_workspace` waits on nothing but the volume |
| `mount_homes`, `AgentConfig.homes_export`, `WS_HOMES_EXPORT`, `Ctx.homes_export` | kept ONE ship, optional and read-only, only to feed migration step 3; deleted next ship with the shares |
| `Engine::ensure_homecache`, `{pool}/homecache/{owner}`, janitor sweep of it, `homecache_volume`, `homecache.not_subvolume` log event and its `deploy/slo.md` row | deleted |
| `ensure_bench_folder`, bench `FolderNotReady`, `bench-folder` hostPath, bench `home` NFS mount | bench gets its own Volume (below) |
| `deploy/k3s/agent-daemonset.yaml` `hostPID` rationale and the Azure Files export env | unchanged this ship (the mount still exists); both go with `mount_homes` next ship |
| `deploy/k3s/workspace-admission.yaml` allow-list entries for `homes/` and `homecache/` | removed; `keys/`, `attach/`, `vol/` stay |
| `tests/ws_e2e.sh` `WS_HOMES_EXPORT` lines, `deploy/k3s/README.md` NFS sections | removed / rewritten |
| `home.persists` SLO (asserts cross-workspace sharing) | replaced by `home.travels` (below) |
| `docs/product/**` `/home/kl/workspaces/{name}` | `/home/kl/workspace` |

The Azure Files shares themselves are retired by hand one ship after this lands (memory
`homes-azure-files` records the names); nothing in code references them once the migration below
has run on every volume.

## Migration (agent-side, per volume, before the first pod start on the new build)

`Engine::migrate_home(worktree)` runs in `apply_workspace` and the bench path before the pod is written, and is a no-op when `~/.kl-home` exists. Otherwise:

1. `mkdir workspace.kl-migrate` in the tree root; `rename` every root entry except `.cache` and
   `workspace.kl-migrate` into it; `rename workspace.kl-migrate workspace`. If a root entry named
   `workspace` already existed (a repo with such a dir) it moves inside like everything else — the
   marker is the layout signal, never the directory name.
2. Move the old subPath dirs to their new homes: `.cache/cargo-registry` → `.cargo/registry`,
   `.cache/{vscode,cursor,zed,windsurf}-server` → `.{vscode-server,cursor-server,zed_server,windsurf-server}`,
   `.cache/jetbrains` → `.jetbrains`. `.cache/xdg/*` → `.cache/`. Missing sources are skipped.
3. Copy the owner's NFS dotfiles once if the export is mounted at `{pool}/homes/{owner}`
   (`cp -a`, everything except `workspaces/`, `bench/` and `.local-cache`, never overwriting an
   entry the tree already has). If the export is not mounted the step is skipped and logged —
   the old NFS home is a convenience copy, not a source of truth, so a workspace must never park on it.
4. `touch ~/.kl-home`; `chown -R 1000:1000` the moved entries only (the tree is already 1000).

Idempotent by the marker. A restore or a clone of a pre-change snapshot produces a tree without
the marker and migrates the same way on its first start, so old snapshots stay usable forever and
nothing is rewritten in place under a snapshot. Every step is a `rename` inside one subvolume, so
step 1 is seconds regardless of tree size. A failure mid-way leaves no marker and is retried on the
next reconcile; step 1 is written so a partially filled `workspace.kl-migrate` is resumed, not
duplicated. `mount_homes` and `homes_export` therefore survive ONE ship, read-only, only to feed
step 3; the `HomeNotReady` gate goes now (a missing mount skips step 3, it never blocks a start).
Log event `home.migrated` with counts of moved entries and whether step 3 ran.

## Bench

A bench is one more thing that owns exactly one subvolume: the bench controller creates a `Volume`
named `bench-{id}` (`replicas: 1`, ownerReference the Bench), claims it the way a workspace does,
and mounts its worktree at `/home/kl`. `BENCH_DIR = /bench` becomes `~/bench` inside the home (a
plain directory the pod creates); the `bench-folder` hostPath and the NFS `home` mount go. A bench
volume is never pushed or cut; it dies with the Bench through Kubernetes GC and the orphan-voldir
sweep. Quota counts its disk like any volume.

## Snapshots, sync, clone, restore

Unchanged in mechanism: the subvolume boundary is the same, only its contents grew. `spec.state`
frozen at a cut is unchanged. Sizes grow by the home's config; caches were already inside.
`git_init_container` clones into `/home/kl/workspace` (the `live` mount is at `/home/kl` in the
init container too; the init container writes `~/.kl-home` and `~/.ssh` is not in the tree).

## sys1-sessions

The in-flight plan (`docs/superpowers/plans/2026-09-22-sys1-sessions.md`, its spec, and
`harness/bench/src/sub.ts`) name `ssh://kl@{ip}/home/kl/workspaces/{name}` for a clone pod's push
and describe `/home/kl` as "the owner's shared NFS home". Both become `/home/kl/workspace` and "the
workspace's home". Nothing else in that programme depends on the layout.

## Probes

- `home.travels` (replaces `home.persists`, same suite and stage, avail 99.9): write a dotfile
  (`~/.config/kl-probe`) and a file under `~/workspace` in workspace A, push, restore into B, read
  both; and assert a FRESH workspace C of the same owner does NOT see the dotfile.
- `ws.cache.travels`: base path `~/.cache` instead of `{ws}/.cache`.
- `ide.serve.up`, `ide.exec`, the file probes: root at `/home/kl/workspace`.
- `homecache.not_subvolume` row removed from `deploy/slo.md` (the catalogue test holds them equal).

## Out of scope

Retiring the Azure Files shares (by hand, next ship). Environments: their volume is not a home and is never migrated; the only
change is dropping the `ensure_homecache` call in `controller/environment/mod.rs`. kl-connect retirement (next programme).
