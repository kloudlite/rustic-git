# Persistent per-person home for workspaces — design

Date: 2026-08-29. Status: draft for review.

## Goal

Everything under `/home/kl` except `~/workspaces/<name>` is the same in every workspace a person
opens, survives pod restarts and workspace deletion, and is replicated to the region's blob
storage so a node move or a region move starts from the last copy rather than from nothing.

## What we have that this builds on

- A person is pinned to **one node per region** by `OwnerBinding` (`crd.rs`
  `binding_name(region, owner)`), and the binding reconciler in the agent already creates the
  person's namespace and policies on that node. Every workspace of that person in the region
  runs on that node. So "shared" inside a region is a **local** problem.
- A `Volume` is one btrfs subvolume on one node with push (snapshot → blob → registry ref
  move), restore, clone and a finalizer that guarantees the subvolume is gone before the object
  is. `Engine::push` stages locally and uploads; `SnapshotRequest` is how a push is asked for.
- Host paths reach pods as statically provisioned `local` PersistentVolumes pinned by node
  affinity (never `hostPath` — PSA `baseline` forbids it); the Nix store is exposed that way
  today, one PV per consumer pointing at one host path.
- Each workspace subvolume is mounted at `/home/kl/workspaces/<name>` (`k8s::workspace_dir(name)`), its own directory so cwd-keyed tool state (Claude Code, opencode sessions) never collides across workspaces sharing the home.

## Design

### Objects

No new CRD and no new API route. The binding reconciler (`bins/agent/src/binding.rs`) creates,
next to the namespace, one child `Volume` named `home-{owner}` with an `ownerReference` to the
`OwnerBinding`, `spec.owner = owner`, `spec.team = ""`, `spec.nodeName = binding.spec.nodeName`,
`spec.quotaGb = binding.spec.homeQuotaGb` (new field, default 2), `spec.source = None`. The
Volume controller then does exactly what it does for a workspace's volume: claims it, creates
`{pool}/vol/home-{owner}/live`, and reports `subvolumePresent`. Deleting the binding deletes the
home volume through the same finalizer path as a workspace's.

Registry name: `vol/{owner}/home-{owner}` — the existing `(owner, id)` keyspace, nothing
special-cased. `GET /v1/volumes/home-{owner}/history` works unchanged for whoever wants to look.

### Mounting

`k8s::workspace_pod` gains a second claim: PV `home-{owner}` (local PV on `{pool}/vol/home-{owner}/live`,
`ReadWriteOnce` — several pods on the same node may share an RWO PV) and PVC `home` in the
person's namespace, mounted at `/home/kl` **before** the workspace claim at `/home/kl/workspaces/<name>`
(kubelet mounts by path depth, so the order is implied by the paths). The PV/PVC are created by
the binding reconciler alongside the Volume, so a workspace pod never waits on them; the
`user-key` Secret mount at `/home/kl/.ssh` and the sshd `authorized_keys` path are unchanged —
a Secret mount inside a PV mount is fine.

Only workspaces mount the home. Environments do not.

### First start and dotfiles

The prelude (`k8s::prelude`) keeps writing `~/.config/zsh/.zshrc` (`ZDOTDIR=~/.config/zsh`, so
every rc file lives under `~/.config`) and `~/.config/fish/config.fish` — but only **if absent** —
and chowns them. The person's edits therefore persist; ours seed once.
`~/workspaces/<name>` is a mount point inside the home; the prelude no longer needs to create it.

### Cache exclusion

At materialization the Volume controller creates nested subvolumes inside the home for
`.cache`, `.npm`, `.cargo/registry`, `.local/share/pnpm` (list in one constant,
`HOME_LOCAL_DIRS`). btrfs `send` skips nested subvolumes and the home's qgroup does not count
them, so caches never upload and never eat the quota. On restore the controller recreates any
missing entries of that list as empty nested subvolumes before the pod starts. A person who
wants something else excluded can `btrfs subvolume create` it themselves — that is the
documented escape hatch, not a UI.

### Replication

Two triggers, both in the agent, both reusing `Engine::push` on the home volume:

1. **Timer** — a beat every `WS_HOME_PUSH_SECS` (default 300) walks the home volumes present on
   this node and pushes each one whose btrfs generation (`btrfs subvolume show … Generation`)
   moved since the value recorded at its last push in `{pool}/vol/home-{owner}/.pushed-gen`.
   Unchanged homes cost one `subvolume show`.
2. **Workspace stop** — `apply_workspace`'s `Stopped` arm creates a `SnapshotRequest` named
   `stop-home-{ws}` for the home volume and gates the pod deletion on it reaching `done`, the
   same fail-closed pattern `apply_environment` uses for its own subvolume. A failed push leaves
   the workspace Running with `Ready=False`, never tears down.

The timer's pushes bypass `SnapshotRequest` on purpose: they are the agent's own housekeeping,
not something anyone asked for, and a request object per five minutes per person would be noise
in `history`. Their commit records carry the message `home: periodic`.

**Pull on first materialization.** When the Volume controller creates a home subvolume on a node
that has none and the registry has a `main` ref for `vol/{owner}/home-{owner}`, it restores that
ref into the new subvolume before reporting `Ready` (the existing "materialize from registry"
path used for a `Volume` with a `source`). A node that already has the subvolume never pulls:
local is truth on its node, the registry is the copy.

**Concurrency.** Inside a region there is one node per person, so two nodes never push the same
home. Across regions each region has its own copy and its own registry ref, and nothing syncs
them; a region move (binding to a node in another region) materializes from that region's
registry, which starts empty — cross-region copy is out of scope and stated so in the UI copy.

### Quota

`homeQuotaGb` on the binding, default 2, becomes the home volume's `quotaGb`, enforced the way
every volume's is (btrfs qgroup). Beyond it writes fail with `ENOSPC`; the workspace list shows
the home's usage next to the workspace's own (`status.usageBytes` already exists on `Volume`).
No warning email, no soft limit.

### Failure modes

| Failure | Behaviour |
|---|---|
| Home PV missing when a pod starts (binding reconciler behind) | Pod stays `Pending` on the PVC; binding reconcile creates it within one `TICK`; no code path starts a workspace without its home. |
| Push fails on the timer | Logged, retried next beat; the subvolume is untouched. |
| Push fails on workspace stop | Workspace stays Running, `Ready=False` with the push error in the condition, like environments. |
| Registry unreachable at first materialization | Volume `phase: Error`, permanent reason `REGION_UNREACHABLE`, workspace waits. Keep-biased: nothing is created empty and then overwritten later. |
| Restore recreates a cache dir that now holds files in the snapshot | Cannot happen: nested subvolumes are never in the send stream. |
| Home over quota | `ENOSPC` inside the pod; push still works (snapshot needs no new space). |

### Not in scope

Per-team shared homes; live cross-node sharing; cross-region sync; a UI for exclusion lists;
home history browsing in the web (the API already answers it).

## Tests

- `crd`/`k8s` unit: `home-{owner}` volume rendering, PV/PVC naming, pod mounts in the right
  order, nested-subvolume list constant used by both create and restore.
- Engine (btrfs, Linux only, in `ws_e2e.sh`): nested cache subvolume excluded from a push;
  restore recreates it; generation check skips an unchanged home.
- Controller (`bins/agent/tests/reconcile.rs`): binding reconcile creates the Volume with the
  binding as owner; Stopped arm gates on `stop-home-{ws}`; timer pushes only changed homes.
- e2e: write `~/.zshrc` in workspace A, open workspace B on the same node, read it; stop A,
  assert a new commit record on `vol/{owner}/home-{owner}`.
