# Workspaces

A workspace is a running Linux sandbox on a Kloudlite region: a pod with your source tree, a Nix
profile of the tools you declared, your persistent home directory, and an sshd behind the
platform's gateway. You ssh in, edit, and run. It is not an image: packages change while it runs,
and the tree survives stops, restarts, and node moves.

## Anatomy

| Path | What | Lifetime |
|---|---|---|
| `/workspace` | The working tree, a btrfs subvolume on the node's pool. Snapshots and clones are cut from it. | the workspace; kept across stop/start |
| `/home/kl` | Your home. One per person and region, on a shared NFS export, so dotfiles and ssh keys follow you into every workspace you own there. | you |
| `/home/kl/.cache`, `~/.local/state`, `CARGO_TARGET_DIR` | Tool caches and shell history, on a local subvolume per (owner, node). Never shared across nodes. | the node |
| `/nix/profile/current` | The Nix profile built from `packages`, on top of the platform's base set. | rebuilt when `packages` changes |

The workspace user is `kl` (uid 1000). The default image is `ghcr.io/kloudlite/kloudlite-workspace`
(Alpine plus sshd); with your own `image`, no sshd and no keys are mounted and you reach the
workspace only through `exec`.

## Packages

`packages` is a list of nixpkgs attribute names, optionally pinned:

```
nodejs            # the region's nixpkgs pin
nodejs@20         # newest 20.x that cache.nixos.org already has a binary for
postgresql@16.4   # exact
jq@latest
```

A pinned entry is resolved and locked when the workspace is written; the lock is kept through
every later edit until you ask for `packages/update`. Nothing is built from source: a pin with no
cached binary is refused (`422`, naming the versions that exist). Installing a package does not
restart the workspace; the new profile is published in place and the shell sees it on the next
`PATH` lookup.

## States and verbs

```
creating ──▶ ready ──▶ stopped ──▶ ready
                │          │
                └──────────┴──▶ deleted
```

| Verb | Route | Effect |
|---|---|---|
| create | `POST /v1/workspaces` | Writes the spec; a node claims it, materialises the tree, builds the profile, starts the pod. |
| start / stop | `POST /v1/workspaces/{id}/start`, `/stop` | Stop cuts a sync point and deletes the pod. The tree stays. Start reschedules; a stopped workspace may start on another node once that node holds the sync point. |
| push | `POST /v1/workspaces/{id}/push` | Takes a **snapshot** of `/workspace`, kept until deleted. The only way to keep state past a delete. |
| clone | `POST /v1/workspaces/{id}/clone` | A new workspace from the source's newest sync point, with the same packages and locks. Seconds, on the same node. |
| restore | `POST /v1/workspaces/restore` | A new workspace from a named snapshot, even after the source is gone. |
| update packages | `POST /v1/workspaces/{id}/packages/update` | Re-resolves the pins; otherwise `packages` is edited in place. |
| attach / detach | `POST /v1/workspaces/{id}/attach`, `/detach` | Connect to an environment. See [Connections](connections.md). |
| delete | `DELETE /v1/workspaces/{id}` | Drops the tree and its sync points. Snapshots from `push` survive, on a detached volume. |

Between pushes a sync beat cuts a point every few minutes from any tree that changed, and peers
replicate it; that is what a clone, a move after a node failure, and the `Replicated` condition
read. It is never a restore target and never listed as history.

## Access

- **ssh**: `kl-connect ws ssh <name>` opens a session through the gateway with your account's ssh
  keys; `kl-connect ws ssh-config` writes `~/.ssh/kloudlite_config` so `ssh <name>` and every
  editor's remote-ssh work unchanged. Keys are per person, projected into every workspace you own.
- **Web console**: `/{owner}/workspaces` lists, creates, starts, stops, pushes and clones; a
  workspace page shows packages, snapshots and the attached environment.
- **API**: everything above, with a bearer token from `kl-connect login`.

## Sizing and limits

A workspace requests 2 CPU / 4 GiB and is limited to 4 CPU / 8 GiB by default; the tree has a
`quota_gb` (the console sends 20). Personal accounts get 5 workspaces, teams 20; see
[Limits and defaults](../reference/limits-and-defaults.md).

## Teams

A workspace created with `team` runs in the team's namespace, under the team's quota, and is
visible to every member. Its ssh keys are still the person's: every member's keys are projected, so
a teammate can ssh into a team workspace.

## Related

- [Connections and intercepts](connections.md)
- [Snapshots](snapshots.md)
- [Workspace API](../reference/api/workspaces.md)
