# A region controller beside the node agents

Status: design, 2026-09-14. Sources: the two read-only audits (`coordination-audit.md`,
`space-env-vet.md`), `docs/superpowers/specs/2026-09-14-person-environment-design.md`,
`bins/agent/src/controller/**`, `bins/agent/src/peer/sweeps.rs`, `bins/agent/src/janitor.rs`,
`crates/workspaces/src/k8s/policies.rs`, `crates/storage/src/ownership/lease.rs`.

## The ruling and the shape

Owner: "can we actually run controllers separately and delegate the tasks to the agents when
needed?" — yes. A new `kloudlite-controller` Deployment per region, leader-elected, ONE active
writer, owns every object that is **shared across nodes or derived purely from spec**. The agent
keeps only what is bound to a host: btrfs, snapshots, replication (`btrfs send`/`receive`), nix
profiles, the two per-pod files (`resolv.conf`, `authorized_keys`), homes/homecache, the local
janitor. The api does NOT take controller work — it stays a writer of spec ("unnecessary
responsibility on api server"). Delegation is through Kubernetes objects only: the controller
writes desired state, the node's agent watches its own node and reports status. No RPC, no queue.

The whole justification is in the audit's own table: seventeen rows say "EVERY node in the region"
for an object that is not per node. Every one of those is either N× write amplification or a
provable disagreement (F3, the `OwnerKeys` `WriteFailed` flap, the `OwnerBinding` per-node fact in
a cluster-scoped field). A single writer deletes the class, it does not patch instances of it.

## Components

| Process | Owns | Holds |
|---|---|---|
| `bins/api` (`user`) | spec of every CR, `OwnerKeys` spec, `SpaceEnvironment` | directory, object store |
| `bins/api` (`admin`) | `Quota`, `Region`, `ClusterSettings`, audit, history | ClickHouse, object store |
| **`bins/controller`** (new) | every cross-node/derived object below | k8s API only — **no disk, no object store, no Azure credential** |
| `bins/agent` (DaemonSet) | this node's bytes and this node's status | btrfs pool, `/nix`, NFS mount, peer listener |

### Single-writer table (every object the audits name)

| Object / artifact | Writers today | New owner | Guard | Migration step |
|---|---|---|---|---|
| Namespace, LimitRange, ResourceQuota, default NetworkPolicies, RoleBindings (`binding.rs:114-185`) | every node | **controller** | SSA under `kloudlite-controller` field manager; body reads `Quota` once per pass | stage 2; agent loses `create/patch` |
| `OwnerBinding.status` (`binding.rs:97-112`) | every node | **controller** for the cluster fact; the per-node fact becomes `status.nodes[node]` written by that node | `conditions_eq` unchanged | stage 2; new status field, additive |
| `OwnerKeys.status` (`keys.rs:102-128`) | every node | **controller** writes `Synced`; each node writes `status.nodes[node] = {generation, ok, reason}` | `settled` guard, whole-second stamps (2026-09-11) | stage 2; agent keeps `ownerkeys/status: patch` on the per-node entry only |
| `{pool}/keys/{owner}/authorized_keys`, key dir sweep | every node | **agent** (unchanged) | content compare, fail-closed | none — host-bound |
| `space-env` egress + `space-{ns}` ingress (`controller/space.rs:143-171`) | every node hosting a pod of the space | **controller**, keyed on the `SpaceEnvironment` alone | SSA + `forget_applied` after every delete | **stage 1**; agent's `converge_space` policy half deleted in the same release |
| `attach-{id}` legacy pair | pod's node | **controller** (collection only) | keep-biased | stage 1 |
| `{pool}/attach/{id}/resolv.conf` | pod's node | **agent** (unchanged) | in place, never renamed | none — host-bound |
| Intercept objects: `intercept-{service}` proxy pod, the Service's selector, the StatefulSet scale, the fixed grant pair (`intercept.rs:267-349`) | environment's node | **controller**, per `2026-09-14-intercept-proxy-design.md` | that spec's | **stage 1** |
| Environment children: StatefulSets, Services, env NetworkPolicies, `env-{id}` ns | environment's node | **controller** | `ensure` + `forget_applied` | stage 2 |
| `Volume.status` Unavailable/Degraded from dead-node & drain sweeps (`sweeps.rs:97-211`) | every live node | **controller** | `replace_status` + idle check, unchanged | stage 3; deletes the documented N× ponytail at `sweeps.rs:306-309` |
| `Volume.spec.nodeName` release, `metadata.finalizers` release (`sweeps.rs:141-158,705-718`) | every sweeping node | **controller** | JSON-patch `test`+`replace`, unchanged | stage 3 |
| `VolumeReplica` DELETE — dead-node reap (`sweeps.rs:24-34`) | every node | **controller** | **+ uid precondition** (the audit's MEDIUM) | stage 3 |
| `Volume` DELETE — unreferenced collection (`sweeps.rs:721-762`) | pin or `preferred_node` | **controller** | uid+resourceVersion preconditions, age floor kept | stage 3 |
| `Snapshot` DELETE — orphan record sweep (`sweeps.rs:420-436`) | every node | **controller** | fresh GET + uid precondition | stage 3 |
| `Snapshot` DELETE — retention (`snapshot.rs:350-385`) | volume's owner node | **agent** (unchanged) | needs local `VolumeReplica` truth | none |
| `{pool}/vol`, `snap/`, `live/` byte sweeps, nix index, homes | own pool only | **agent** (unchanged) | fresh GET before every delete | none |
| `{pool}/attach/{id}` REMOVE (`janitor.rs:161-183`) | every node, own pool | **agent**, keep-set fixed | live Workspace+Bench ids; unavailable list ⇒ sweep nothing | **stage 0** |
| Claim / un-place / take_volume (`claim.rs`, `volume.rs:510`) | the nodes | **agent** (unchanged) — see §Placement | CAS, unchanged | none |
| Workspace/Environment/Bench/Volume `.status` (ordinary), pods, Secrets, labels | owner node | **agent** (unchanged) | forced SSA + no-op compare | none |
| Bench pod force-delete on another node (`bench.rs:106-201`) | any node | **controller** | `unplaceable()` predicate; the folder lock stays the real fence | stage 3 |
| `Node` decommission-status annotation | the node | **agent** (unchanged) | sticky `drained` | none |

## The leader lease

Two candidates.

`crates/storage/src/ownership/lease.rs` is proven (conditional puts, epoch as a fencing token,
10 s TTL / 3 s renew, "the store is the arbiter, never the clock and never the ordinal"), but it
needs an `ObjectStore` — an S3 or Azure credential in a process the ruling says holds no disk and
no object store, and a second failure domain for a controller whose only dependency is the API
server it is already watching.

**Recommended: `coordination.k8s.io/v1` Lease** named `kloudlite-controller` in `kube-system`,
written with the same semantics: `holderIdentity` = pod name, `leaseTransitions` playing the epoch,
`renewTime` + `leaseDurationSeconds: 15` the expiry, and the resourceVersion CAS on the `update`
playing object_store's `UpdateVersion`. Renew every 5 s (three per TTL, `LEADER_RENEW`'s rule), take
over only when `renewTime + duration` has passed on the taker's own read of the object. Failover
~15 s, matching the ruling. `k8s-openapi` 0.28 already carries the type; no new dependency.

The fencing rule carries over verbatim and is the thing not to lose: **every write checks the epoch
it was elected under**, and a write that finds a newer `leaseTransitions` demotes rather than
finishing. Unlike SlateDB there is no writer fence underneath, so the API server's own
resourceVersion CAS on each object is the backstop — which is why every controller write stays a
CAS or an idempotent SSA, never a blind force over an unread object.

## Placement stays in the agents

Recommendation: **claims, un-placement and `take_volume` stay where they are; only the sweeps move.**

`may_claim` (`claim.rs:44-53`) is decided from the claiming node's OWN `VolumeReplica.branches` —
the node is the authority on what bytes it holds, and the claim is settled by a `replace_status`
CAS that already admits exactly one winner. Moving it would (a) make the controller read every
replica row in the region to decide something the node knows locally, (b) make placement stop
entirely while the controller is down, where today a node can still pick up its own work, and
(c) put the unknown-cache rule in a worse place: a controller with an unlisted cache would be
deciding for the whole region at once, instead of one node declining to claim.

The **sweeps** are the opposite case and move in stage 3: the dead-node, drain, retire, reap and
collect passes each compute a CLUSTER-wide verdict, every node computes the same one, and the audit
already documents the N× amplification and a `preferred_node` workaround as the upgrade path. A
single elected writer IS that upgrade, and it removes the workaround along with three MEDIUMs
(reap precondition, `hosted` staleness, beat-old parent listing — the controller sweeps from its own
reflectors with a fresh GET per delete, as today).

`drop_stale_worktrees` does not move: it deletes this node's own bytes. It keeps the fresh
`Volume` GET, which is already its whole guard.

## The delegation contract

The controller never touches disk, so everything it wants a node to do is a spec/desired field the
node already watches. Nothing new is needed for stage 1 and 2 — the existing field selectors
(`spec.nodeName` on Volume, `status.nodeName` on the parents) are the delegation. Stage 3 adds one
field: `Volume.spec.nodeName` cleared by the controller instead of by whichever node swept, which
the admission policy already admits for the agent and must now admit for the controller.

**Per-node status entries.** Two cluster-scoped fields hold per-node facts today and must be split:

```yaml
# OwnerKeys.status
nodes:
  - node: aks-pool-0
    generation: 1757800000000
    ok: true
    reason: Applied            # written ONLY by that node
conditions: [ {type: Synced, ...} ]   # written ONLY by the controller, folded from nodes[]
```

`OwnerBinding.status` takes the same shape: `nodes[].namespaceReady` per node, the `NamespaceReady`
condition folded by the controller. This is what closes the audit's two MEDIUMs — a node whose disk
is bad can no longer patch `WriteFailed` over a healthy node's `Applied` (`keys.rs:102-128`), and
"namespaces exist" stops being asserted cluster-wide by a node that only checked itself
(`binding.rs:47-77`). Folding rule: `Synced=True` only when every node listed is at the current
generation; a node absent from the list is not a failure (it may have no pod of that owner).

## Intercept

The owner's finding — a teammate's workspace or bench, following the same environment through its
own `SpaceEnvironment`, dials `api:8080` and is dropped, because `intercept_ingress`
(`policies.rs:54-73`) admits only `env_ns` while the endpoint now lives in another space's namespace
— is answered by a proxy pod in the environment namespace, not by granting every following space a
path into someone else's workspace. That design is its own spec,
`docs/superpowers/specs/2026-09-14-intercept-proxy-design.md`.

What belongs here: **the controller owns the intercept objects** — the `intercept-{service}` proxy
pod, the Service's selector, the StatefulSet scale, and the fixed grant pair (env-namespace egress
to the one workspace pod, workspace-pod ingress from the env namespace on the intercepted ports).
One writer, in stage 1, for the same reason as the space policies: these are cross-namespace objects
and the environment's node is not the only thing that can change them.

Fan-out: one `SpaceEnvironment` event re-renders ONE environment — the one its spec names, plus the
one it named before, read from the controller's own cache. That is the fix for the vet's F5, where
`run.rs:434-437` wakes every environment on every node today.

## The other ≥MEDIUM findings, decided

| Finding | Decision |
|---|---|
| F1 CRITICAL `space.rs:137/151/160/165/171` — deleted policy unrecreatable for 600 s | gone with the code: the controller owns these policies, and every delete it makes calls `forget_applied` (as `intercept.rs:280` already does). Stage 1. |
| F2 HIGH — env-side prune reads a reflector unordered with its trigger | gone: one process, one cache, one writer. The prune becomes "render the derived set", not "delete what another node's cache disagrees with". Stage 1. |
| F3 HIGH — per-pod condition gate deletes the namespace-wide `space-env` | gone: no per-pod gate exists in the controller; the object is a pure function of the `SpaceEnvironment`. Stage 1. |
| F4 MEDIUM — `store_ready` never un-readies | **fixed in the shared code, both processes**: `Ctx` records the last successful watch event/relist; `spaces()`/`workspaces()`/`environments()` answer `None` past 2× `WATCH_TIMEOUT_SECS`. One timestamp. Stage 1. |
| F6 MEDIUM — multi-writer flap of `space-*` under cache lag | gone with the single writer. Stage 1. |
| audit: `Volume.status` sweep marks by every node | controller. Stage 3. |
| audit: reap without uid precondition | controller + uid precondition. Stage 3. |
| audit: `ResourceQuota` body depends on a live `Quota` read | controller, one reader. Stage 2. |
| audit: `ensure`'s 600 s memory | **keep.** It was per-process and the fleet had N of them; with one writer the memory is now the truth for the whole region, which is strictly better. The rule stands unchanged and gets a test: any path that mutates a child outside `ensure` calls `forget_applied` first. |
| audit: bench pod delete on another node | controller; the `harness-bench` folder lock stays the fence — never weaken it to "the delete fences it". |
| audit: `hosted` from a beat-old listing | stays with the agent (own bytes), fresh GET unchanged. |
| vet F6/F7 LOW (`condition` vs `condition_since`, per-pod Environment GET) | folded into stage 1's move — the controller reads the environment from its cache, so F7 disappears; use `condition_since`. |

## RBAC

Agent **loses**: `namespaces`, `limitranges`, `resourcequotas`, `networkpolicies` (all verbs),
`services`, `rolebindings`, `clusterroles: bind`, `statefulsets`,
`ownerbindings: create`, and `ownerkeys/status` narrows to the per-node entry (enforced by the
admission policy, since RBAC cannot say "this field"). It keeps every CR read, its own `/status`
writes, finalizers, labels, `pods`, and the Volume spec carve-outs.

Controller **gets** exactly what the agent lost, plus `coordination.k8s.io/leases` on its own name,
plus `get/list/watch` on every CR. It gets **no** node-local anything: no hostPath, no privileged
context, no `WS_PEER_SECRET`, runs as an ordinary uid with a read-only root.

`agent-rbac.yaml`'s header table IS the role — every removed row moves to a new
`deploy/k3s/controller-rbac.yaml` with the same call-site column, and a call added without a row
still 403s naming the file.

## Admission policy

`agent-admission.yaml` keeps its shape and gains a second `matchConditions` arm for the controller
identity: the controller may write spec of the objects it owns (`Volume.spec.nodeName` for the
sweep release only) and may NOT write `Workspace`/`Environment`/`Bench`/`Quota` spec — that is
`/v1`'s, and the split is the same "RBAC, not convention" rule. The agent's existing
`Volume.spec.restoreTo` carve-out is untouched. New rule: on `ownerkeys/status` and
`ownerbindings/status`, the agent may change only `status.nodes[?node == its own node]` — a CEL
comparison against `oldObject`, which is the only way to express the per-node entry.

## Failure modes

- **Controller down**: nothing converges, nothing breaks. Every object it owns is level-triggered
  and already applied; pods keep running, grants keep granting, DNS keeps resolving, agents keep
  snapshotting and replicating. New workspaces stall at `NamespaceReady`, a switched space keeps
  the old grant until the controller returns. Failover is ~15 s, so this is a tail case, not a mode.
- **Leader flap**: a demoted leader stops at its next epoch check; the successor re-renders the same
  bytes, and SSA of identical bytes is a server-side no-op — no storm (the 2026-09-11 rule: identical
  bytes, whole-second `lastTransitionTime`).
- **Agent down**: host work stalls for that node ONLY — its snapshots, its profiles, its two files.
  Everything cross-node keeps converging, which is better than today, where a dead node also stopped
  being one of the N writers of shared objects.
- **Split brain**: impossible for a shared object (Lease CAS + epoch check + per-object CAS) and
  irrelevant for a host object (one node holds the disk).
- **Controller cache unlisted**: same rule as the agents — `None`, decide nothing, delete nothing.
  This matters more now, so the freshness timestamp (F4) ships with stage 1, not after it.

## Observability

No new vocabulary. The controller emits the same `reconcile.pass` / `status.written` /
`reconcile.done` / `reconcile.slow` / `event.seen` / `event.late` fields the agent does, with
`node` replaced by `holder`. Three additions: `leader.acquired` / `leader.lost` (holder, epoch,
reason) at info, and `grant.rendered { environment, peers, intercepts }` — the vet's 3am note was
that `space.grant.pruned` did not say which value it read, and a derived-set render replaces the
prune entirely. History rows are unchanged; `admin/workloads` gains the Deployment so a `Mark::Boot`
setting rolls it.

SLO ids to add (`crates/workspaces/src/slo/catalogue.rs` + `deploy/slo.md`, held equal by the test):

| id | SLI | suite |
|---|---|---|
| `ctl.leader` | Exactly one controller holds the lease, and a delete of the holder pod elects another within 30 s | Hourly |
| `ctl.grant.switch` | Switching a space's environment moves both grant halves within 10 s and the old one is gone | Fast |

## Deploy shape

`deploy/k3s/controller.yaml` — `kube-system`, Deployment, **one replica**, `Recreate` strategy, no
PDB. Recommendation: no PDB and one replica, because a second replica is a hot standby that does
nothing but shorten a 15 s failover, and `Recreate` plus the lease already bounds a roll to one TTL.
A PDB with `minAvailable: 1` on a single-replica Deployment blocks node drains — exactly the
operation this fleet performs deliberately — so it would cost more than it buys. Add the second
replica if a measured failover ever hurts.

Image: same SHA as the other Rust binaries, pinned by `deploy/pin.sh` alongside them.

## Staged rollout

**Stage 0 — janitor keep-set (ruling C), independent, ships first.**
`janitor_sweep_attach` derives liveness from `read_dir(vol/)`, but a clone or restore SHARES its
source's volume (`controller/volume.rs:768-793`), so `vol/{clone-id}` never exists and a live pod's
`resolv.conf` directory is swept after an hour. Fix: the keep-set is the live Workspace + Bench ids
from the cluster listing the janitor already makes for benches; the `vol/` read stays only as a
belt. Keep-biased: if either list cannot be read, sweep nothing (today an unreadable `vol/` returns
0, so the shape already exists). No RBAC change, no mixed-build concern. Test: a unit case with a
clone id present in the Workspace set and absent from `vol/`.

**Stage 1 — controller runs the space policies and the intercept objects; fixes F1, F2, F3, F5, F6
and carries the intercept-proxy spec's cutover.**
New binary, leader lease, three reflectors (`SpaceEnvironment`, `Environment`, `Workspace`), the
derived grant renderer. The agent's policy code in `converge_space` and `intercept_policies` is
**deleted in the same release** — never two writers across a roll. Mixed-build safety: the two
writers produce identical bytes under different field managers, and SSA with `.force()` converges
either way, so an agent rolled late is harmless; the ordering that matters is the RBAC apply, which
must land AFTER the agents are rolled (an agent that still has the code and lost the verb 403s and
aborts its reconcile — the `networkpolicies: delete` lesson in `agent-rbac.yaml`). The F4 freshness
timestamp ships here, as does the intercept-proxy spec's own cutover. Tests: policy-rendering unit
tests (a switch removes the old half, ports-only, empty-ports admits nothing), the agent reconcile
suite loses its policy assertions, and the new integration test below.

**Stage 2 — namespaces, LimitRange, ResourceQuota, RoleBindings, `OwnerKeys`/`OwnerBinding` status.**
The per-node status entries are additive and ship one release before the fold, so an old agent's
whole-object write is still valid while the new field is being populated. RBAC applies after the
agent roll, same rule. Tests: `binding` unit tests move wholesale; a two-agent integration case
asserts one node's `WriteFailed` does not flip another's `Applied`.

**Stage 3 — sweeps and their writes.** Dead-node, drain, retire, reap, collect, bench force-delete.
This is the only stage where a mixed build is genuinely unsafe (two sweepers with different verdicts
are what the CAS survives but the write amplification is the point of the move), so it ships behind
a `ClusterSettings` `Mark::Boot` flag read by both: agents sweep while it is off, the controller
when it is on, flipped after the DaemonSet reports fully rolled — the same gate `agents_read_spaces`
already uses for the attach migration. Placement is NOT in this stage and is not planned.

## Tests

- **Unit, per module**: lease (acquire, renew, takeover after expiry, epoch check refuses a demoted
  write), space-policy rendering (idempotent, a switch removes the old half),
  status folding (absent node ≠ failure), janitor keep-set.
- **Agent reconcile suite** (`bins/agent/tests/reconcile/`): every case that asserted a policy or a
  namespace write loses that assertion and gains "the agent wrote nothing here"; the per-node status
  cases assert the agent touches only its own entry.
- **Integration, two fake agents**: an envtest-style harness with the controller plus two stub agents
  that only write per-node status. Asserts: one leader at a time across a forced failover; a space
  switch re-renders both halves and deletes none of the new ones; a `SpaceEnvironment` created on one
  node's objects wakes exactly one environment, not every environment on both; a node reporting
  `WriteFailed` leaves the cluster condition `True` while the other node is current.

## Open questions

1. Does the controller roll with the region (one per k3s cluster) or is there ever a case for one
   per AKS region too? This spec assumes one per cluster, named by region.
2. Stage 3's `Mark::Boot` flag is an extra knob that exists for one release — acceptable, or ship
   stage 3 as a hard cutover with a documented maintenance window?
3. `OwnerKeys.status.nodes[]` grows with the node count for every owner. Cap it (drop entries for
   nodes that no longer exist, on the controller's fold) — or is a `Synced` condition plus a log
   line enough, with no per-node entry at all?
4. Should the controller also own `Snapshot` retention? It is cluster-wide reasoning over
   `VolumeReplica` rows, but the owning node has the local truth. This spec leaves it in the agent.
5. Is the bench pod force-delete worth moving at all, given the folder lock is the real fence?
