# Pre-binding local volumes from the PV side — design

Date: 2026-08-30. Status: draft for review.

## Problem

Creating a workspace or clone waits ~13 s, and nearly all of it is one claim binding. Sampled twice
a second through a real clone:

```
   6 ms  pvc=none   cond=WaitingForStorage  pod=none
1151 ms  pvc=Pending
12820 ms pvc=Bound                          ← 11.7 s waiting to bind
14002 ms                                    pod=Pending   ← the gate released 1.2 s after the bind
16338 ms                                    pod=Running
```

The pod-after-binding gate is doing its job — it creates the pod 1.2 s after the claim binds and
there are no `FailedScheduling` events. The wait is upstream of it.

**The binding time is a lottery.** Measured across seven runs of the same shape: 0.4 s, 0.8 s,
2.5 s, 8.2 s, 11.7 s, 12.3 s. That spread against the PV controller's 15 s resync is the signature
of a claim that sometimes binds on the event and sometimes falls through to the periodic sync.

**The cause is which side we pre-bind from.** `k8s::claim` sets `volume_name` on the PVC. A claim
naming its volume opts out of delayed binding, so it never takes the path `kloudlite-git-local`
(`volumeBindingMode: WaitForFirstConsumer`) is configured for, and is left to the controller's own
schedule.

## What the pre-binding is for

It cannot simply be dropped. Every `nix-*` PV points at the same `/nix`, and the `live-*` PVs differ
only by path; with identical class, size and access mode, a binder free to choose could match one
workspace's claim to another workspace's volume. Something must make the pairing exact.

`claimRef` on the PV does exactly that, from the other side: it names one namespace and one claim,
and no other claim can bind that volume. Same exclusivity, without defeating delayed binding.

## Measured on the live cluster

| Shape | Result |
|---|---|
| `volumeName` on the PVC (today) | binds in 0.4–12.3 s, unpredictable |
| `claimRef` on the PV, no `volumeName`, **no consumer pod at all** | **561 ms** |
| `claimRef`, full PV + PVC + pod to `Running` | **1.3 s, 2.0 s, 2.1 s, 2.0 s** — zero `FailedScheduling` |

Two facts that shape the design:

1. **A `claimRef` claim binds without a consumer** (561 ms above). Binding is driven by the PV
   controller on the PV, not deferred to pod scheduling. So the pod-after-binding gate does **not**
   deadlock under this change — it simply releases in under a second.
2. **Workspace pods go through the scheduler.** `workspace_pod` leaves `spec.nodeName` unset (its
   own test asserts `node_name.is_none()`); placement comes from the PV's node affinity, and the
   live pod's assignment is stamped by `default-scheduler`. Nothing here bypasses scheduling.

## Design

`k8s::local_pv` gains a `claimRef` naming the namespace and claim it is for. `k8s::claim` drops
`volume_name`. Both are already built as a pair by `ensure_storage`, which knows both names, so
neither signature needs to grow.

Everything else stays:

- **The pod-after-binding gate stays.** It costs nothing when claims bind in under a second, and it
  keeps the `FailedScheduling` backoff from reappearing if a bind is ever slow again. It is already
  written, tested and deployed.
- The agent's `get` on `persistentvolumeclaims` stays — the gate reads them.
- Node affinity, access modes, capacity, reclaim policy: unchanged.

### The PV becomes create-only

`ensure_storage` applies the PV today with server-side apply. That must not continue for the
`claimRef` field: a PV that is already **bound** carries a `claimRef` the binder filled in with a
`uid` and `resourceVersion`, and applying our own `{namespace, name}` over it would strip those and
invite the controller to re-evaluate a live binding.

So the PV is created when absent and left alone when it exists. That matches what a PV already is —
`nodeAffinity` is immutable, so an apply could never have changed the thing that matters anyway.
Existing bound PVs are untouched by this change and keep working exactly as they do now.

### Migration

No migration and no cutover:

- Existing PV/PVC pairs are already `Bound`. Nothing re-applies them, nothing unbinds them, and the
  workspaces using them are unaffected.
- New pairs — every workspace and clone created after the deploy — get the new shape and the fast
  path.
- The two shapes coexist indefinitely. A `volumeName` claim and a `claimRef` claim are both exactly
  pinned to one volume; only the binding latency differs.

## Expected result

A clone goes from ~13 s to roughly **2–3 s**: ~0.5–1 s to bind, up to one `BIND_POLL` (2 s) for the
gate to notice, then ~1 s to start the pod. The dominant remaining term becomes the gate's own poll
interval, which is a knob we own rather than a controller resync we do not.

## Failure modes

| Failure | Behaviour |
|---|---|
| A PV exists from before this change (no `claimRef`, claim has `volumeName`) | Left alone; already bound; the workspace keeps working. Only new pairs take the new path. |
| A PV exists but is unbound and lacks `claimRef` | Its claim still carries `volumeName` (it was created together with it), so it binds the old way. Mixed pairs never occur: both objects are created in one `ensure_storage` call. |
| `claimRef` names a claim that is later deleted and recreated | The recreated claim has a new uid; the PV's `claimRef` has no uid, so it matches by name and binds. This is why `claimRef` is written WITHOUT a uid. |
| Two PVs accidentally name the same claim | Only one binds; the other stays `Available` with a stale `claimRef`. Cannot arise here — the PV name and the claim name are both derived from the same id. |
| A claim is created before its PV | The gate holds the pod; the PV arrives in the same `ensure_storage` call microseconds later and binding proceeds. |
| Binding is slow anyway | Unchanged from today: the gate holds the pod and requeues, so the scheduler never sees an unbound claim. |

## Not in scope

The storage class and its binding mode (already `WaitForFirstConsumer`). The PV controller's resync
period — this removes our dependence on it rather than tuning it. The gate, `BIND_POLL`, and the
RBAC that supports them. Reducing the NUMBER of claims per workspace (that is the separate
shared-claims design).

## Tests

- `crates/workspaces` units: `local_pv` emits a `claimRef` with the namespace and claim name and no
  uid; `claim` emits no `volume_name`; the pair built by one `ensure_storage` call names each other
  consistently.
- `bins/agent/tests/reconcile.rs`: a PV that already exists is not re-applied (create-only); a
  workspace whose PV and claim are absent gets both, and the claim reaches the gate as before.
- Measured, on the cluster, after deploy: a clone reaches `Ready` in single-digit seconds with zero
  `FailedScheduling`, repeated at least three times — one run is noise, and the previous round of
  this work produced a 2 s outlier that three repeats corrected to 13–15 s.
