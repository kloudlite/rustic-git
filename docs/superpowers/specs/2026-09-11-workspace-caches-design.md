# Where a workspace's caches live (spec)

Status: implemented 2026-09-11 (8f94cb87, then the addendum below).

## Addendum (2026-09-11, owner's decision): every cache travels with the workspace

The three-way sort below was superseded the same day: "we need to move everything that can be
cached into the workspace folder." Everything §"Stay on homecache" lists — the package stores,
`RUSTUP_HOME`, `GOMODCACHE`, `XDG_CACHE_HOME`, the `~/.cargo/registry` mount and every editor
server mount — moved to `{ws}/.cache/`; the mounts became `live` subPath mounts of that directory.
homecache keeps only `TMPDIR` and shell state. This is the platform standard and is documented
as such in `docs/product/concepts/workspaces.md`.

## Why

A clone, a restore, or a start on another node arrives with an empty `target/`: the workspace
directory is snapshotted and replicated, but Cargo's build output was redirected out of it, to
the node-local `homecache`. For a Rust workspace that empty `target/` is the slowest thing the
platform does to a person — and the redirect was never about the workspace dir. It came with
the NFS home: caches must not live on the shared export. Everything that had to leave the home
landed in one "local scratch" bucket, per-project build output included.

This spec sorts every path the pod redirects today into three homes by one rule each, moves
per-project build output back into the workspace dir, and ignores what the platform places there
from git globally so no repository ever sees an unasked diff.

## The three homes and the rule for each

| Home | Backed by | Rule | Survives |
|---|---|---|---|
| NFS home `/home/kl` | Azure Files export, one per person per region | small config and state; written rarely | everything: every workspace, every node, restarts |
| workspace dir `/home/kl/workspaces/{name}` | btrfs subvolume, snapshotted by push, replicated | the project's files and whatever is derived FROM them (build output, indexes) | stop/start, clone, restore, node move — arrives warm |
| homecache `/home/kl/.local-cache` | node-local btrfs subvolume per (owner, node) | big, global, project-independent, rebuildable in seconds | restarts on the same node only |

Today the workspace dir holds only what the project itself puts there; homecache holds the rest,
and pays for it on every move.

## The sort

### Move into the workspace dir (per project; snapshotted)

| Variable | Today | New |
|---|---|---|
| `CARGO_TARGET_DIR` | `{homecache}/cargo-target` | `{ws}/.cache/cargo-target` |
| `GOCACHE` | unset → `$XDG_CACHE_HOME/go-build` on homecache | `{ws}/.cache/go-build` |
| `PLAYWRIGHT_BROWSERS_PATH` | `{homecache}/playwright` | `{ws}/.cache/ms-playwright` (browser versions are pinned by the repo) |
| graft index | — | `{ws}/graft/` (owned by `kl ide serve`, see that spec) |
| direnv | — | `{ws}/.direnv/` (direnv's own default) |

Under `{ws}/.cache/`, not the tool defaults (`./target`), so nothing the platform places can
collide with a directory a repository keeps under version control, and the global ignore is
three lines. Tools that assume `./target` (`cargo run` prints paths; some scripts) see the
variable, which every Cargo-aware tool honours.

Already in the workspace dir with no redirect, untouched: `node_modules/`, `.venv/`, `.next/`,
`.turbo/`, `dist/`, `__pycache__/`, project-level `.gradle/`.

### Stay on homecache (global, rebuildable)

`XDG_CACHE_HOME`, `npm_config_cache`, `PNPM_STORE_DIR`, `BUN_INSTALL_CACHE_DIR`, `UV_CACHE_DIR`,
`PIP_CACHE_DIR`, `DENO_DIR`, `GOMODCACHE`, `RUSTUP_HOME`, the `~/.cargo/registry` mount, the
editor server mounts (`~/.vscode-server`, `~/.cursor-server`).

Added, same rule: `YARN_CACHE_FOLDER`, `COMPOSER_CACHE_DIR`, `NUGET_PACKAGES`, `MAVEN_OPTS`
`-Dmaven.repo.local={homecache}/m2`, `TMPDIR={homecache}/tmp`; editor server mounts for
`~/.zed_server`, `~/.windsurf-server`, `~/.jetbrains` (backends the IDE downloads, hundreds of MB).

Noted, not changed: `GRADLE_USER_HOME` also holds `gradle.properties` credentials, the same
shape as `CARGO_HOME`; it is on homecache today and a node move loses those credentials. The
right split is `GRADLE_USER_HOME` on the home and the project cache in `{ws}/.gradle` (Gradle's
own default) — a one-line change, included.

### Stay on the NFS home (small config and state)

`ZDOTDIR`, `GIT_CONFIG_SYSTEM`, `CARGO_HOME` (`credentials.toml`, `config.toml`), `~/.ssh`,
`~/.config/*`, `~/.gitconfig`, `GRADLE_USER_HOME` (moved back, above). `HISTFILE` and
`~/.local/state` stay on homecache/state as they are: per-node write traffic, not config.

## Git ignores at the global level

The image ships `/home/kl/.config/git/ignore` — git's default `core.excludesFile`, no config
needed — with exactly what the platform itself places inside a workspace dir:

```
# kloudlite: derived state the platform places inside a workspace directory
.cache/
graft/
.direnv/
```

Never a per-repository `.gitignore` line: that is a diff the person did not ask for, in every
repository they open. Never the project's own artefacts (`node_modules/`, `dist/`): a repository
that forgot to ignore those should see the mistake, not have it hidden.

The file lives on the image, not the NFS home, so a person's own `~/.config/git/ignore` cannot
be silently overwritten: if the home already has one, the entrypoint appends the block once
(idempotent, marker comment) instead of replacing it. `kl ide serve` refuses to start when the
block is missing.

## Quota

The default workspace quota is 20 GB, sized for a source tree. With build output inside it a
Rust or Node workspace needs more: default `quota_gb` becomes 50 for a new workspace (console
default and `DEFAULT_WS_QUOTA_GB`), the per-owner disk ceiling stays as it is (100 GB person /
400 GB team), and the console shows `.cache/` size on the workspace page so the number is
visible. An existing workspace keeps its quota; raising it is the existing `quota_gb` edit.

## Sync and replication cost

btrfs snapshots are copy-on-write: N cuts of a 20 GB `target/` cost 20 GB plus the deltas. What
crosses to a replica on each sync beat is the changed bytes, which for an incremental build is
tens to a few hundred MB. Acceptable, and it is what makes the replica warm. A person who does
not want build output replicated may make `{ws}/.cache` its own subvolume: a nested subvolume is
excluded from its parent's snapshot, and the platform never touches it. Documented, not a knob.

## Migration

Nothing moves. A running workspace keeps its env until its next start; on start the new env
points Cargo at an empty `{ws}/.cache/cargo-target` and the first build is a full one, once. The
old `{homecache}/cargo-target` and `playwright` directories are reclaimed by the janitor's
existing homecache sweep. No CRD change, no data migration.

## Changes

- `crates/workspaces/src/k8s/workspace.rs` `login_env`: the variable table above; the
  `GRADLE_USER_HOME` split; `TMPDIR`.
- `workspace_pod`: three editor-server mounts added.
- `Dockerfile` workspace stage: `/home/kl/.config/git/ignore` block, entrypoint append-once.
- `crates/workspaces/src/crd/snapshot.rs` `DEFAULT_WS_QUOTA_GB` 20 → 50; console default.
- `docs/product`: `concepts/workspaces.md` table (what persists where) and `reference/limits.md`.
- Probe: `ws.cache.travels` (hourly) — a workspace builds a trivial Cargo crate, is cloned, and
  the clone's `{ws}/.cache/cargo-target` is non-empty before any build.
- `CLAUDE.md` "Every person has one persistent home" paragraph: the three-homes rule.

## Open decisions (yours)

1. Default quota 50 GB, or keep 20 and let the person raise it?
2. `PLAYWRIGHT_BROWSERS_PATH` in the workspace dir (pinned per repo, ~1 GB) or leave on homecache?
3. `~/.cargo/registry` and `RUSTUP_HOME` cost minutes to rebuild on a node move. Leave on
   homecache (this spec), or a later per-region shared read-mostly cache?
