# Fewer storage objects per workspace — design

Date: 2026-08-30. Status: draft for review.

## Goal

Cut the Kubernetes storage objects a workspace costs, without changing what a workspace can see.

Today a workspace pod mounts four claims:

| Claim | Scope | Backing host path | Objects per… |
|---|---|---|---|
| `live-ws-{id}` | per workspace | `{pool}/vol/{id}/live` | workspace |
| `nix-ws-{id}` | **per workspace** | `/nix` — the same path for every one | **workspace** |
| `home` | per namespace | `{pool}/vol/home-{owner}/live` | user |
| `attach` | per namespace | `{pool}/attach` | user |

`live` is genuinely per workspace: each names a different subvolume. The other three do not —
`nix` is the odd one out, and `attach` carries a whole PV+PVC pair for one small file.

Two phases, sequenced. Phase 1 is the larger saving and lands first.

## Verified facts this rests on

Checked on the live cluster, not assumed.

1. **Every `nix-ws-{id}` PV points at the same host path, `/nix`.** Two workspaces in one namespace
   have two PV objects with identical `local.path`. Nothing in Kubernetes notices they overlap;
   it is safe only because the mount is `ReadOnlyMany`.
2. **A PVC may be mounted by many pods.** Both workspace pods in `ws-karthik1729` already mount the
   same `home` claim. The 1:1 binding is PV↔PVC, never PVC↔pod — which is what the existing
   `nix_pv_name` doc comment conflates when it concludes "one per workspace".
3. **All five CRDs are cluster-scoped** (`kubectl api-resources`: `NAMESPACED=false`). So a
   `Workspace` or `OwnerBinding` is a valid owner for both a cluster-scoped PV and a namespaced PVC.
4. **The `nix` pair is owned by the Workspace**; the `home` and `attach` pairs by the OwnerBinding.
   Garbage collection therefore already removes a workspace's `nix` pair when the workspace goes.
5. **A ConfigMap cannot carry the resolv.conf.** Mounted with `subPath` it never updates — verified:
   the ConfigMap was changed and the pod still read the old content two minutes later. Without
   `subPath` it cannot be mounted at all, because a ConfigMap volume is a directory and
   `/etc/resolv.conf` is a file. Kubelet updates such volumes by swapping a symlink, and a
   `subPath` bind-mount holds the old inode — the same reason a host-side rename never reaches a
   running pod.
6. **There is no way to point a container's resolver at a different path.** glibc and musl read
   `/etc/resolv.conf` at a hardcoded location; `RES_OPTIONS` and `LOCALDOMAIN` tune options and
   search, not the file. The mount is the only lever.

## Phase 1 — one `nix` claim per namespace

`nix-{ns}` PV + `nix` claim, `ReadOnlyMany` over `/nix`, authored by `binding::ensure_home`'s
namespace loop beside the `home` and `attach` pairs. `workspace_pod` mounts the claim `nix`.
`nix_pv_name`/`nix_claim_name` are replaced by `nix_pv_name(ns)` and a `NIX_CLAIM` constant, and
the `ensure_storage` call leaves `apply_workspace`.

Saving: **2 objects per workspace** become 2 per namespace. A user with five workspaces goes from
10 nix objects to 2.

### Migration — attrition, no sweep

Existing `nix-ws-{id}` pairs are owned by their Workspace, so they are collected when it is
deleted. Nothing has to retire them:

- A running pod keeps mounting `nix-ws-{id}` until it is next recreated. Pods are create-if-absent,
  so nothing recreates them on the roll; they keep working untouched.
- The next pod creation for that workspace — a stop/start, or a node move — mounts `nix` instead.
- The stale pair then sits bound and unused until the workspace is deleted, when GC takes it.

An unused bound PV costs nothing but a line in `kubectl get pv`. A sweep is deliberately **not**
built: it would be new machinery to reclaim objects that already have an owner.

The one ordering rule: the `nix` claim must exist before a pod that mounts it. `ensure_home` runs
before `write_binding_status`, and `apply_workspace` already gates pod creation on
`namespace_ready`, which is the same ordering the `home` and `attach` claims rely on today.

## Phase 2 — back the resolv.conf mount with the workspace's own claim

Drop the `attach-{ns}` PV and `attach` claim. Write the rendered file inside the workspace's own
subvolume and mount it from the claim already there:

```
live-ws-{id}  →  subPath: .kloudlite/resolv.conf  →  /etc/resolv.conf   (read-only)
```

The agent writes `{pool}/vol/{id}/live/.kloudlite/resolv.conf` instead of
`{pool}/attach/{id}/resolv.conf`. Everything else about attachment is unchanged: the same renderer,
the same in-place write, the same two NetworkPolicies, the same no-restart behaviour. Multiple
mounts from one volume with different subPaths are ordinary Kubernetes.

Saving: **2 objects per user**, and `attach_root`/`attach_dir`/`attach_pv_name`/`ATTACH_CLAIM` and
the janitor's `janitor_sweep_attach` all go with it — the directory being swept no longer exists.

### What this costs, stated plainly

Platform state moves inside user data. The file travels into every snapshot, push, clone and
restore of that workspace, and it is visible in the user's tree at
`~/workspaces/{name}/.kloudlite/resolv.conf`.

More importantly it becomes **user-writable territory**: the prelude chowns the workspace directory
to `kl`, so the person in the workspace can delete `.kloudlite` outright. That is survivable but must
be deliberate:

- Deleting it while the pod runs changes nothing — the pod holds the inode, exactly as fact 5
  explains.
- The agent rewrites the file on every reconcile and *before* pod creation, so the next start
  restores it.
- If the path is a directory (the failure a missing `subPath` target creates), the reconciler's
  existing recovery branch removes it and writes the file.

A stale copy arriving via clone or restore is corrected on the first reconcile, because the
renderer is keyed on the destination workspace's namespace, not on what the file said.

This is the phase to drop if the review decides platform state belongs outside user volumes. Phase 1
stands on its own.

## Failure modes

| Failure | Behaviour |
|---|---|
| Pod mounts `nix` before the binding created it | Pod stays `Pending` on the claim, as it already does for `home`. The `namespace_ready` gate makes this the same window that exists today. |
| A workspace pod predating phase 1 is running | Keeps mounting `nix-ws-{id}`; unaffected until its next recreation. |
| The old `nix-ws-{id}` PVC is deleted while a pod mounts it | `pvc-protection` holds it `Terminating` until the pod goes. Nothing in this design deletes it; only workspace deletion does, which deletes the pod too. |
| Phase 2: user deletes `~/workspaces/{name}/.kloudlite` while running | DNS keeps working — the pod holds the inode. Restored before the next pod start. |
| Phase 2: user replaces the file with a directory | The reconciler's existing directory-recovery branch removes it and rewrites the file before the pod is created. |
| Phase 2: clone or restore carries a stale resolv.conf | Rewritten on the first reconcile of the destination workspace. |
| `/nix` missing on a node | Unchanged from today: the PV points at a path the node must have; the mount fails and the pod does not start. |

## Not in scope

Changing `live` (genuinely per workspace). Consolidating PVs across namespaces (a local PV binds to
exactly one claim, so one claim per namespace is the floor). Reclaiming the stale `nix-ws-{id}`
pairs early. Any change to how attachment resolves names, to the NetworkPolicies, or to the
renderer.

## Tests

- `crates/workspaces` units: `nix_pv_name(ns)` and `NIX_CLAIM` naming; `workspace_pod` mounts the
  claim `nix` and no longer a per-workspace one; phase 2 — the `/etc/resolv.conf` mount names the
  `live` claim with `subPath: .kloudlite/resolv.conf`.
- `bins/agent/tests/reconcile.rs`: the binding reconciler creates the `nix` pair with the binding's
  ownerReference, in every namespace it already creates `home` for; `apply_workspace` no longer
  creates a nix pair; phase 2 — the reconciler writes the file inside the live subvolume and the
  attach sweep is gone.
- `crd_yaml.rs` unaffected — no CRD field changes in either phase.
- `tests/ws_e2e.sh`: the existing attachment phase is the end-to-end proof for phase 2; it must
  pass unchanged, which is the point — the mechanism moves, the behaviour does not.
