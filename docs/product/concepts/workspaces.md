# Workspaces

A workspace is one running pod with your source tree, a Nix profile of the packages you asked for, and ssh. One workspace per change you are making; they are cheap to create, clone, and delete.

## What is in a workspace

| Path | What | Persists |
|---|---|---|
| `/home/kl/workspaces/{name}` | Your tree, a btrfs subvolume sized by `quota_gb` | Across stop and start; snapshotted by push |
| `/home/kl` | Your home for the region: dotfiles, editor state, credentials | Across every workspace of yours in the region |
| Tool caches, shell history | `~/.cache`, `~/.local/state`, `CARGO_TARGET_DIR`, and their kin | Per workspace and node, on local disk |
| `PATH` | The packages in `packages`, plus a base set | Rebuilt from the spec on every start |

The tree is what you version. The home is what makes the second workspace feel like the first. Caches are local because they are large and never worth moving.

## Image

Default image is `ghcr.io/kloudlite/kloudlite-workspace`, which carries the ssh server, Nix, and the `kl` CLI. A custom `image` may replace it; it then owns its own ssh setup and mounts no `authorized_keys`.

## Packages

`packages` is a list of nixpkgs attribute names. Bare `nodejs` means the region's nixpkgs pin; `nodejs@22` is locked to the newest 22.x that the binary cache holds, and the lock is frozen into the workspace before it is created. See [Packages](../workspaces/packages.md).

## State

```
creating ─▶ ready ⇄ stopped ─▶ deleted
              │
              ▼
            error
```

| State | Meaning |
|---|---|
| `creating` | Placed and being built; `packages_status` and `ssh` fill in as they arrive |
| `ready` | The pod is running and accepting ssh |
| `stopped` | The pod is gone; the tree is kept and a sync point was cut |
| `error` | The controller could not converge; `degraded` says why |
| `deleted` | Being torn down; the volume survives only if a snapshot references it |

## Placement

A workspace is placed on one node in its region. Its tree is replicated to other nodes on a sync beat, so a stopped workspace may start on any node that already holds its newest sync point. A running one is never moved; if its node dies it is interrupted, and the way forward is a [clone](../workspaces/clone-and-restore.md) from the last synced point.

## Seeding from a repository

`repo` and `branch` on create run a clone inside the pod with your platform ssh key, into `/home/kl/workspaces/{name}`. No credential is minted or stored for it.

## Next steps

::: cards
- [Create a workspace](../workspaces/create.md) — Every field of the create request.
- [Lifecycle](../workspaces/lifecycle.md) — Start, stop, delete, and what each keeps.
- [ssh](../workspaces/ssh.md) — Connect from a terminal, an editor, or an agent.
:::
