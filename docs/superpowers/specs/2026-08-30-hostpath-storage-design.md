# Replacing local PVs and PVCs with host mounts — design

Date: 2026-08-30. Status: draft for review.

## Problem

A workspace's storage is expressed as four PersistentVolume/PersistentVolumeClaim pairs, all of
them static, all of them pointing at a directory the agent already created on the node's btrfs
pool. Kubernetes contributes nothing to their lifecycle: `kloudlite-git-local` is
`no-provisioner`, so nothing is provisioned; the paths are chosen by the agent, not the cluster;
and the capacity numbers are cosmetic, since the real quota is a btrfs qgroup.

What the PV/PVC layer does contribute is machinery:

| Piece | What it exists for |
|---|---|
| `k8s::local_pv`, `k8s::claim` | build the pair |
| `ensure_storage` (4 call sites) | create the pair, create-only on both halves |
| `claims_bound` / `first_unbound_claim`, `BIND_POLL` | hold the pod until the pair binds |
| `WaitingForStorage` condition | report that wait |
| `STORAGE_CLASS` | a class that provisions nothing |
| RBAC: `persistentvolumes`, `persistentvolumeclaims` | let the agent write them |
| `claimRef` pre-binding | stop the binder pairing the wrong volume with the wrong claim |

Every row is a consequence of the row above it. The binding step is the root: because binding is
asynchronous and the binder is free to choose, we need exclusivity (`claimRef`), a gate to stop the
pod racing the bind, a poll interval for that gate, and a condition to explain the wait.

None of that describes anything true about the storage. The directory exists before the pod does,
on a node we have already chosen, at a path we computed.

## Design

Mount the directories directly. A pod names the host path it needs; placement is expressed on the
pod rather than borrowed from a PV's node affinity.

```
live     {pool}/vol/{id}/live          → the workspace's subvolume
nix      /nix                          → the store (read-only)
home     {pool}/vol/home-{owner}/live  → the owner's home
attach   {pool}/attach/{ws}/resolv.conf → the rendered resolv.conf (read-only)
```

These are the same four paths `ensure_storage` passes to `local_pv` today. Nothing about the
on-disk layout, the btrfs subvolumes, the qgroups, or the registry changes.

### Placement moves to the pod

Today `workspace_pod` deliberately leaves `nodeName` unset and placement rides on the PV's
`nodeAffinity`; `PodContext.node_name` is already threaded to every builder for exactly that
purpose (`k8s.rs:959` — "placement rides on the PV; kept in context for the PV builder"). The value
is already in hand.

Placement becomes a `nodeSelector` on `kubernetes.io/hostname`, added beside the
`kloudlite-git.io/session` selector and toleration the pods already carry.

**`nodeSelector`, not `nodeName`.** Setting `nodeName` assigns the pod directly and bypasses the
scheduler, which would also bypass resource packing, taints, and every admission the scheduler
performs. The selector keeps the scheduler in the loop and expresses the same constraint.

### `hostPath` type is load-bearing

Every mount declares an explicit `type`:

- directories: `type: Directory` — the kubelet fails the mount if the path is absent
- `resolv.conf`: `type: File`

This is the one place the change is a genuine improvement rather than a simplification. An untyped
`hostPath` **creates** a missing path as an empty directory, which is how a pod that lands on the
wrong node silently presents a wiped workspace. `type: Directory` turns that into a mount failure
the pod reports, so a placement bug is loud instead of destructive.

### subPath mounts collapse

Four mounts use `subPath` today only because one claim serves several purposes:

| Mount | today | after |
|---|---|---|
| `/nix/store` | `nix` claim, `subPath: store` | `hostPath: /nix/store` |
| profile | `nix` claim, `subPath: var/kloudlite/profiles/{id}` | `hostPath: {PROFILES_DIR}/{id}` |
| `/etc/resolv.conf` | `attach` claim, `subPath: {id}/resolv.conf` | `hostPath: {pool}/attach/{id}/resolv.conf` |
| user mounts | `live` claim, `subPath: volumes/{folder}` | `hostPath: {pool}/vol/{id}/live/volumes/{folder}` |

A host path can name the deeper directory directly, so the indirection goes away. `valid_segment`
on `Mount::folder` stays exactly as it is — it is what keeps a user-supplied name a single safe
segment, and it is doing that job today regardless of how the mount is expressed.

### The resolv.conf in-place write still matters

The attach file is written in place and never renamed, because a pod holds the inode. That is
unchanged and remains load-bearing: with `hostPath` the kubelet bind-mounts the file itself, so a
rename would leave the pod reading the old inode exactly as it does now.

## What gets deleted

- `k8s::local_pv`, `k8s::claim`, `k8s::attach_pv_name`, `k8s::nix_pv_name`, `k8s::pv_name`,
  `k8s::claim_name`, `k8s::nix_claim_name`, `HOME_CLAIM`, `ATTACH_CLAIM`, `STORAGE_CLASS`
- `controller::ensure_storage` and its four call sites
- `controller::first_unbound_claim`, `BIND_POLL`, the gate in `apply_workspace`, and the
  `WaitingForStorage` condition
- the `claimRef` pre-binding shipped in `e722a3e`, which exists only to make binding exclusive
- `persistentvolumes` and `persistentvolumeclaims` from the agent ClusterRole
- the `kloudlite-git-local` StorageClass manifest
- four PV objects and four PVC objects per workspace

Create time loses the binding term entirely: what is left is the pod start the gate was waiting on.

## Costs

### PSA must drop below `baseline`

`baseline` forbids `hostPath`. Workspace and environment namespaces move to
`pod-security.kubernetes.io/enforce: privileged`, with `audit`/`warn` left at `restricted` so the
gap stays visible.

This is the real price, and it is namespace-wide rather than mount-specific: after it, nothing
structurally prevents a future mount in those namespaces from naming a different host path. The
guarantee changes from "the API server refuses it" to "our code does not do it". What continues to
constrain the user is unchanged — `validate_mount`'s `valid_segment` on the folder, and the fact
that every host path is agent-constructed from `pool` and `id`.

The pods' own hardening (`hardened()` security context, `runtime_class`, the NetworkPolicies) is
untouched; only the namespace-level admission floor moves.

### Every running workspace restarts once

A pod's volumes cannot be patched, so a pod built against PVCs has to be recreated to mount host
paths. The controller creates pods with `create_if_absent`, so it will not replace one on its own.

Migration is therefore explicit and one-time: a reconcile that finds a workspace pod carrying a
`persistentVolumeClaim` volume deletes it, and the next pass recreates it in the new shape. That is
one restart per running workspace, taken at roll time.

The orphaned PV and PVC objects are then swept — the PVCs are namespaced and go with their
namespace or by name; the PVs are cluster-scoped and named `pv-*`, `nix-*`, `home-*`, `attach-*`.
The sweep is keep-biased like the janitor's siblings and runs only against objects carrying our
owner label.

### Node affinity stops being enforced by two parties

Today the scheduler checks the PV's affinity and the kubelet checks nothing; after, the scheduler
checks the pod's selector and the kubelet checks `type: Directory`. That is arguably stronger — the
kubelet's check is at mount time, on the node, against reality — but it is a different shape and
worth stating.

## Not in scope

The btrfs layout, subvolumes, qgroups, the registry, snapshot/push semantics, the profile index,
the attach NetworkPolicies, and `validate_mount` itself. The home volume's `DEFAULT_HOME_QUOTA_GB`
inconsistency noted during the claimRef review is a separate pre-existing bug.

## Failure modes

| Failure | Behaviour |
|---|---|
| Pod scheduled to a node without the subvolume | `type: Directory` fails the mount; the pod reports it. Today this is a silent empty directory. |
| The subvolume is not yet materialized | Same — the mount fails and the pod restarts. The Volume reconcile creates it, so this resolves on its own; today the gate covered this by holding the pod. |
| `nodeSelector` matches no node | `FailedScheduling`, named and visible, instead of a bind that never completes. |
| A pre-migration pod still mounts PVCs | The migration sweep deletes it once; the next pass rebuilds it. |
| PSA rejects the pod | Only if the namespace label was not moved. Fails closed and loudly at create. |
| A stale PV is left behind | Harmless — it binds nothing, since no claim references it. Swept by name. |

## Tests

- `crates/workspaces` units: every workspace and service pod mounts `hostPath` sources with an
  explicit `type` and no `persistentVolumeClaim` volume; the four paths match the ones
  `ensure_storage` passes today; `nodeSelector` carries `kubernetes.io/hostname`; the existing
  assertions that pods carry no `hostPath` are inverted, and the ones about `valid_segment` stay.
- `bins/agent/tests/reconcile.rs`: a workspace reconcile writes no PV and no PVC; a pod carrying a
  PVC volume is deleted once and recreated; a workspace already in the new shape is left alone.
- RBAC test: the agent ClusterRole no longer grants `persistentvolumes` or
  `persistentvolumeclaims`.
- Measured, on the cluster, after deploy: a clone reaches `Ready` with no binding term, repeated at
  least three times; a pod forced onto the wrong node fails its mount rather than starting with an
  empty `/home/kl`.
