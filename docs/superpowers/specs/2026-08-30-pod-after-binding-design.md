# Creating a workspace pod only once its claims are bound — design

Date: 2026-08-30. Status: draft for review.

## Problem

Since profile reuse landed, a clone reaches `Ready` in ~29 s of which almost none is our work:

| Step | Time |
|---|---|
| Workspace created → Volume `Ready` (the btrfs clone) | 0 s |
| → `PackagesReady` ("reused a profile already on this node") | **0 s** |
| → pod object created | 0 s |
| → pod `Scheduled` | **~27 s** |
| → `Ready` | ~1 s |

The pod is created immediately and then sits unschedulable:

```
FailedScheduling: 0/3 nodes are available: pod has unbound immediate PersistentVolumeClaims
```

Nothing is broken — the claims bind and the pod starts — but the wait is now the whole cost of
creating a workspace.

**This cost was previously hidden.** Before profile reuse, `ensure_storage` created the claims, the
28 s nix evaluation ran, the claims bound during it, and by the time the pod was created they were
ready. Removing the evaluation did not remove the wait; it uncovered it. The measured saving on a
clone was therefore ~1 s, not ~28 s.

## Two measured facts

1. **A pre-bound PVC takes ~12 s to bind.** Timed on the live cluster with a PV and PVC shaped
   exactly like ours: **12,345 ms** from `kubectl apply` to `phase: Bound`. That is the PV
   controller's resync period (`--pvclaimbinder-sync-period`, default 15 s) — a claim created just
   after a sync waits for the next one.
2. **The scheduler then adds its own backoff.** The pod was created at `:43`, the claims bound
   ~12 s later, and `PodScheduled` did not happen until `:09:10` — ~27 s total. An unschedulable
   pod is retried on the scheduler's own cadence, so the binding completing does not promptly wake
   it.

So ~12 s is a floor imposed by a cluster-level controller, and ~15 s is scheduler backoff we
provoke by creating the pod too early.

## Why the claims cannot simply use WaitForFirstConsumer

`kloudlite-local` already IS `WaitForFirstConsumer`. The scheduler nevertheless calls these claims
"immediate" because `k8s::claim` sets `volume_name`, and a claim that names its PV opts out of
delayed binding.

That pre-binding is load-bearing and must stay. Every `nix-*` PV points at the same `/nix`, and the
`live-*` PVs differ only by path — with matching class, size and access mode, a binder free to
choose could match one workspace's claim to another workspace's PV. Naming the volume is what makes
the pairing exact. This design does not touch it.

## Design

`apply_workspace` gains one gate, immediately before it creates the pod: every claim the pod will
mount must be `Bound`. While any is not, the reconcile writes a `Progressing` condition and
requeues on a short interval instead of creating the pod.

The claims are the ones the pod already mounts: `live-{id}`, `nix-{id}` (per workspace today), and
`home` and `attach` (per namespace, and normally bound long before, since the binding reconciler
creates them).

The requeue interval is its own constant, deliberately much shorter than `TICK` (15 s): waiting for
a bind is a poll for something that lands within seconds, and reusing `TICK` would replace a 15 s
scheduler backoff with a 15 s reconcile delay and save nothing. Two seconds is the value here — it
costs at most a handful of no-op reconciles per workspace creation and lands the pod within ~2 s of
the bind.

Nothing else moves. `ensure_storage` already runs before `ensure_profile`, so the claims are created
as early as they can be; the gate only stops the pod racing ahead of them.

### What this is expected to buy

The scheduler's first attempt succeeds, so the ~15 s backoff disappears:

| | now | after |
|---|---|---|
| Clone with an indexed profile | ~29 s | **~15 s** |
| New workspace, cold profile | ~30 s + build | build + ~15 s |

The ~12 s binding floor remains. It is a cluster-level controller's resync, not something the agent
can shorten; the only lever is `--pvclaimbinder-sync-period` on the k3s control plane, which is out
of scope here and would want its own measurement.

This is a smaller number than "single-digit seconds", and saying so is the point: the honest
expected result is roughly halving workspace creation, not eliminating the wait.

## Failure modes

| Failure | Behaviour |
|---|---|
| A claim never binds (no matching PV, wrong node affinity) | The workspace stays `Progressing` with the claim named in the condition, requeuing. Today it instead creates a pod that sits `Pending` with the same underlying problem — the failure becomes visible on the Workspace rather than only on the pod. |
| A claim is deleted after the gate passes | Unchanged from today: the pod is created and the kubelet fails to mount; the next reconcile re-creates the claim. |
| `home` or `attach` not yet bound because the binding reconciler is behind | Same gate, same requeue. Already covered for `home` by the existing readiness gate; this makes it uniform. |
| The pod already exists | The gate is skipped entirely — it guards creation, not existence, so a running workspace is never disturbed by a claim briefly reporting unbound. |
| Reading a claim's status fails | Treated as not-yet-bound and requeued, never as bound. A transient API error must not let the pod race ahead. |

## Not in scope

Changing `volume_name` pre-binding, the storage class, or its binding mode. Tuning the PV
controller's resync period. Reducing the number of claims per workspace (that is the separate
shared-claims design, which would remove `nix-{id}` and leave one new claim per workspace instead of
two — it reduces how many bindings must complete, not how long one takes).

## Tests

- `bins/agent/tests/reconcile.rs`: a workspace whose claims are `Pending` gets no pod and a
  `Progressing` condition naming the unbound claim; once they report `Bound` the pod is created;
  a workspace whose pod already exists is unaffected by an unbound claim; a claim whose status
  cannot be read is treated as unbound.
- Measured, on the cluster, after deploy: a clone with an indexed profile reaches `Ready` with no
  `FailedScheduling` event on its pod. That absence is the check — the event is what this exists to
  remove.
