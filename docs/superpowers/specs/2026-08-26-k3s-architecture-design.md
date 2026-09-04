# k3s for workspace and environment orchestration — architecture design

**Status:** proposed
**Replaces:** `docs/superpowers/specs/2026-08-25-direct-container-runtime-design.md` (bollard/Podman
direct runtime control — cancelled, see "What this deletes")
**Touches:** `bins/agent/`, `crates/workspaces/src/{model.rs,api.rs,scheduler.rs,lease.rs,engine/compose.rs}`,
`bins/server/src/vol_agent.rs` (job routes only), `deploy/`
**Does not touch:** `crates/workspaces/src/engine/{ops.rs,blob.rs,pool.rs,fsck.rs}`,
`crates/workspaces/src/cosmos.rs`, the registry `vol/{owner}/{id}` surface
**Related:** `docs/superpowers/audit-2026-08-25.md` (C1, H1, H2, M1)

## What is actually being changed

The product is the btrfs snapshot/push/clone engine plus the Cosmos control plane. That is
`engine/ops.rs` (896 lines of snapshot, send/receive, block layers, lineage, staged-push crash
recovery), `engine/blob.rs`, `engine/pool.rs`, and the `vol/{owner}/{id}` registry surface in
`bins/server/src/vol_agent.rs`. **None of it changes.** A `Snapshot`, a `LineageEntry`, a
`CommitRecord`, `push`/`clone`/`restore` semantics — all identical after this work.

What changes is the thin, badly-earning layer that decides *what runs where, and when*: today
`bins/agent/src/container.rs` (102 lines shelling `docker run`/`start`/`stop`/`rm`),
`crates/workspaces/src/engine/compose.rs` (124 lines rendering YAML for `docker compose`), the
owner→node binding scheduler in `crates/workspaces/src/scheduler.rs`, and — the larger half —
the bespoke job queue: `Job`/`JobKind`/`JobState`, the 120s lease with no renewal, the requeue
sweep, the agent's long-poll and report loop. Together those are a hand-rolled, half-finished
orchestrator with a scheduler, a work queue, a lease manager, and a reconciler in it. k3s is a
finished one, and the audit's H1/H2/H4/H7 are all bugs in the half we wrote ourselves.

We were about to make that hand-rolled orchestrator *bigger*: the design this replaces added a
bollard reconcile loop, per-environment bridge networks, a per-node IPAM block allocator with
hand-picked `/26` subnets, and — reading its "cross-machine: deliberately none" section
honestly — a placement rule contorted specifically so we would never have to build an overlay.
That is the shape of a project that ends in WireGuard mesh and route distribution. Adopting k3s
buys all of it (CNI, DNS, IPAM, scheduling, reconciliation, health, restart policy) for the cost
of running a control plane we do not currently run.

## Cluster shape

k3s, **SQLite datastore** (the k3s default), **single server**. No etcd, no HA control plane.

This is a deliberate ceiling. A single k3s server with SQLite means the control plane is a
single point of failure for *scheduling and API access*; it does **not** stop running pods, and
it does not touch data — a workspace's bytes are on a worker node's btrfs pool and in Azure
blob storage, neither of which the k3s server participates in. Losing the server for twenty
minutes means "no new workspaces, no lifecycle changes" — the same blast radius today's single
`bins/api` process already has. Restore is a file copy of `/var/lib/rancher/k3s/server/db/`.

```
control-plane node   k3s server, no workloads       node-role.kubernetes.io/control-plane:NoSchedule
session node         32 OCPU / 128 GB, btrfs pool   kloudlite-git.io/role=session
env node             16 OCPU / 128 GB, btrfs pool   kloudlite-git.io/role=env
```

Listing labels are a reconciled view, not a record. `kloudlite-git.io/owner` and `kloudlite-git.io/kind`
are stamped by `/v1` at create AND re-stamped by the node controller on every reconcile, because a
label is how a list stays indexed while `spec.owner` stays the truth. An object whose label is
missing is owned correctly and invisible to `/v1`'s list — so the controller heals it rather than
trusting whoever wrote the object. Authorization never reads the label.

Node roles are label + taint, both:

- Label `kloudlite-git.io/role=session|env` — the *positive* half; every workspace pod carries a
  `nodeSelector` for it.
- Taint `kloudlite-git.io/role=session:NoSchedule` (and `=env`) — the *negative* half; nothing
  lands there without a matching toleration.

A label alone lets a stray pod (a debug shell, a DaemonSet, an operator) drift onto a workload
node; a taint alone lets our pods land anywhere. Both, or the isolation is decorative. The
control plane carries the standard `node-role.kubernetes.io/control-plane:NoSchedule` and we
never tolerate it outside k3s' own components.

Sizing means the session node is the one that has to be defended: 32 OCPU across N user
workspaces, all running user-chosen images. See "Resource requests and limits".

## CRDs: the Kubernetes API becomes the reconcile substrate

Three custom resources, all cluster-scoped except where noted:

| CRD | Spec (desired) | Status (observed) |
|---|---|---|
| `Volume` (`volumes.kloudlite-git.io`) | `owner`, `nodeName`, `quotaGb`, `source` (empty / clone-of / restore-of `{volume, snapshotId}`) | `phase`, `subvolumePresent`, `lineageTip`, `lastPush{commit,sha,at}`, `conditions` |
| `Workspace` (`workspaces.kloudlite-git.io`) | `owner`, `name`, `region`, `image`, `volumeRef`, `nodeName`, `desiredState` (Running/Stopped), `resources` | `phase`, `podRef`, `conditions` |
| `Environment` (`environments.kloudlite-git.io`) | `owner`, `name`, `region`, `services[]` (the existing `Service` struct verbatim), `volumeRef`, `nodeName`, `desiredState` | `phase`, `serviceStatus[]`, `conditions` |

`Volume` is separate from `Workspace`/`Environment` on purpose, and it is the load-bearing
choice in this design. Both a workspace and an environment own exactly one btrfs subvolume with
identical semantics — that symmetry is already in the code (`JobKind::Push` and
`JobKind::WsClone` both branch on which payload key is present,
`bins/agent/src/lib.rs:385,415`, precisely because the two shapes are the same operation). One
`Volume` CRD with one controller ends that branching. The btrfs lifecycle reconciles against
`Volume`; the container lifecycle reconciles against `Workspace`/`Environment`.

**`Service` and `Mount` are reused as-is** (`crates/workspaces/src/model.rs:198,222`) as the
CRD's embedded schema — same serde types, now with an OpenAPI schema generated from them so the
API server validates structure. `validate_mount` (`model.rs:211`) additionally runs in a
**ValidatingAdmissionWebhook**, so a hostPath escape is refused by the cluster itself and not
only by our API handler. That is a strictly stronger position than today: C1 becomes
unexpressible via any client, including `kubectl`.

## The agent becomes a node-level controller (DaemonSet)

`bins/agent` stops being a job worker and becomes a set of **controllers** — watch, reconcile,
converge — running as one privileged pod per worker node. The long-poll loop
(`run_with_engine` in `bins/agent/src/lib.rs`), `register`, `/vol-agent/work`, and the report
path all go away.

**Each node's controller watches only its own objects.** The watch carries a field selector on
`spec.nodeName` (the same mechanism the kubelet itself uses to watch only its own pods), so
node `session-0` never sees, queues, or reconciles `env-0`'s volumes:

```
watch Volume      where spec.nodeName == $NODE_NAME
watch Workspace   where spec.nodeName == $NODE_NAME
watch Environment where spec.nodeName == $NODE_NAME
```

`$NODE_NAME` comes from the downward API (`spec.nodeName` fieldRef). This is what replaces the
lease: **two nodes cannot contend for the same object, because the object names its node.**
There is no acquisition, no expiry, no requeue sweep — the assignment is a field, not a claim.

Why a DaemonSet rather than folding btrfs work into k8s itself: **btrfs send/receive is not a
container operation.** It is a privileged syscall sequence against a specific host filesystem,
with a per-volume `flock` (`engine::ws_lock`) serializing it. There is no k8s object for
"snapshot this subvolume and stream it to Azure". One privileged pod per node owning the pool
is the correct shape, and it is exactly what the current agent already is — only its trigger
changes.

**No leader election.** A controller sharded by `spec.nodeName` with one replica per node has
exactly one candidate per shard; `coordination.k8s.io` Leases would be guarding against nothing.
The one place a Lease *would* be needed is a future cluster-wide controller (the admission-side
placement decider, if it ever leaves the API); note it there, do not build it here.

```yaml
# deploy/kloudlite-git-agent.yaml (sketch — the parts that are load-bearing)
kind: DaemonSet
spec:
  template:
    spec:
      hostPID: false
      nodeSelector: {}            # runs on every worker; tolerates both role taints
      tolerations:
        - { key: kloudlite-git.io/role, operator: Exists, effect: NoSchedule }
      containers:
        - name: agent
          securityContext:
            privileged: true      # see below — this is not reducible today
          volumeMounts:
            - { name: pool, mountPath: /mnt/wspool, mountPropagation: Bidirectional }
            - { name: dev, mountPath: /dev }
          env:
            - { name: WS_POOL, value: /mnt/wspool }
            # still the server tier: the Engine's RegistryClient pushes commit records and
            # moves `vol/{owner}/{id}` refs there. That surface is unchanged by this design.
            - { name: WS_REGISTRY_URL, value: http://kloudlite-git-srv:8081 }
            - { name: NODE_NAME, valueFrom: { fieldRef: { fieldPath: spec.nodeName } } }
      volumes:
        - { name: pool, hostPath: { path: /mnt/wspool, type: Directory } }
        - { name: dev, hostPath: { path: /dev } }
```

**`privileged: true` is honest, not lazy.** The narrower story would be
`CAP_SYS_ADMIN` + `CAP_DAC_READ_SEARCH` + a device mount, but the agent also calls `mount`/
`umount` (`cleanup_local`'s loop-mount unmount, `engine::is_mountpoint`) and attaches loop
devices for block layers (`Pool::img`), and loop-device attach needs more than the capability
list suggests. Attempt the reduction empirically once the DaemonSet is running; do not block
the migration on it. Record the ceiling with a `ponytail:` marker on the securityContext.

`mountPropagation: Bidirectional` is required and easy to miss: the agent creates loop mounts
under `/mnt/wspool` that must be visible in the *host* mount namespace, because workspace pods
bind that host path independently. Without it, a block-restored workspace's pod sees an empty
directory where the agent sees data.

**The agent's identity is the node.** `AgentDoc`, `{pool}/agent-id`, and
`POST /vol-agent/register` all disappear: a controller's identity is `$NODE_NAME`, and its
liveness is the DaemonSet pod's own readiness probe — which the cluster already watches. This
also retires the audit's "restarted agent whose persisted id no longer exists server-side loops
on 404 forever" bug and the ~90-Cosmos-ops-per-agent-per-30s idle heartbeat cost (P5). A watch
is a single long-lived connection with no polling at all.

### RBAC

One ServiceAccount per agent DaemonSet. Cluster-scoped for the CRDs it watches (a field
selector is not an authorization boundary — the RBAC grant is cluster-wide even though the
watch is narrowed), namespaced for the workloads it creates:

| Verb set | Resources | Scope |
|---|---|---|
| get, list, **watch** | `volumes`, `workspaces`, `environments` (`*.kloudlite-git.io`) | cluster |
| update, patch | `volumes/status`, `workspaces/status`, `environments/status` | cluster |
| get, list, watch, create, update, delete | `deployments`, `services`, `configmaps`, `networkpolicies`, `pods` | namespaces `env-*`, `ws-*` |
| get, list, watch, create, delete | `namespaces` | cluster |
| get, list, watch | `nodes` | cluster (own node's labels/allocatable) |
| create | `events` | cluster (reconcile errors as k8s Events) |
| create | `pods/exec` | `ws-*` namespaces only — the workspace access path |

Note what is **absent**: no `update`/`patch` on the CRDs' *spec* subresource. The status
subresource split (`/status`) is what enforces the source-of-truth rule mechanically —
controllers can write observations and cannot write desired state, because RBAC forbids it.
That is the rule from "Two writable copies" below, expressed as a grant rather than a
convention.

`namespaces: create` at cluster scope is the one broad grant and it is unavoidable if the node
controller owns namespace lifecycle. Splitting it into a cluster-wide namespace controller is
worth doing if the grant ever bothers us; it does not for v1.

`pods/exec` is a real privilege: it is arbitrary code execution inside any workspace pod. It is
also the product's access path (`default_ws_image`'s doc: "`docker exec` is the v1 access
path"). Scope it to `ws-*` and never grant it on `env-*`.

## Object model: what a workspace and an environment become

| Domain concept | Today | Under k3s |
|---|---|---|
| Workspace/Environment doc | Cosmos item | CRD object (`spec` desired, `status` observed) |
| btrfs subvolume | implicit, per `{pool}/vol/{id}` | `Volume` CRD, one per workspace/environment |
| Environment (`model.rs:243`) | compose project `env-{id}` | Namespace `env-{id}` |
| `Service` (`model.rs:222`) | compose service | Deployment + Service (ClusterIP) in that namespace |
| Service-to-service DNS | compose's embedded resolver | CoreDNS: `db.env-{id}.svc.cluster.local`, and plain `db` within the namespace |
| `Mount` (`model.rs:198`) | bind `live/volumes/{folder}:{path}` | hostPath volume `{{pool}}/vol/{id}/live/volumes/{folder}` at `{path}` |
| Workspace (`model.rs:77`) | container `ws-{id}` | Deployment (replicas 1) in namespace `ws-{id}` |
| Workspace access | `docker exec ws-{id}` | `kubectl exec` / `pods/exec` API |

`mongodb://db:27017` keeps resolving, from CoreDNS instead of compose — one of the few things
compose genuinely provided, and the reason the replaced design was going to hand-roll
aardvark-dns configuration.

**`model.rs` barely changes.** `Service`, `Mount`, `Environment`, `Workspace` are already the
right shape; `render` in `compose.rs` is replaced by a function producing k8s objects from the
same structs. `validate_mount` (`model.rs:211`) stays exactly as it is and stays load-bearing:
a hostPath source is every bit as dangerous as a docker bind source, and C1 is a hostPath escape
under k8s just as it was a bind escape under compose. If anything the rule tightens — a pod
spec is submitted to an API server that will happily mount `/` for us.

Workspace as a Deployment rather than a bare Pod: a bare Pod that dies stays dead, and
`spec.desiredState` maps cleanly onto `replicas: 1` / `replicas: 0`, which is also how a stop
survives a node reboot. `restart: unless-stopped` in `container.rs:46` becomes exactly this,
and `JobKind::WsStart`/`WsStop` stop existing as jobs at all — the user edits desired state and
the controller converges.

## Storage: the pod must schedule where the data is

This is the single hardest invariant and it deserves being stated plainly.

**A workspace's data is one btrfs subvolume on one node's local disk. It cannot move. Therefore
the pod cannot move.** Nothing about k8s changes this; k8s only gives us a vocabulary for it.

The mechanism is hostPath + nodeAffinity, the standard local-storage pattern:

```yaml
spec:
  affinity:
    nodeAffinity:
      requiredDuringSchedulingIgnoredDuringExecution:
        nodeSelectorTerms:
          - matchExpressions:
              - { key: kubernetes.io/hostname, operator: In, values: ["session-0"] }
  volumes:
    - name: live
      hostPath: { path: /mnt/wspool/vol/{id}/live, type: Directory }
```

`values` comes from the `Volume` CRD's `spec.nodeName`, which the admission path wrote (see
"Who decides placement"). It is the same fact `Workspace.placement` (`model.rs:85`) holds today,
written by `scheduler.rs`'s `place_ws` — the field survives, in a different store, with the same
meaning. The pod's affinity is *derived* from the `Volume`, never chosen independently: two
places allowed to name a node is two places that can disagree about where the data is.

`type: Directory` (not `DirectoryOrCreate`) is deliberate: if the subvolume is missing, the pod
must fail to start loudly, not silently run against an empty directory that k8s created. Audit
H3 is precisely this failure mode under docker — an environment whose subvolume vanished was
"silently rebuilt empty even though its pushed history exists in the registry". A hard failure
here converts that class of bug into a visible one.

**Are local PersistentVolumes worth it?** Not yet, and possibly never. A local PV would give us
`nodeAffinity` declared once on the PV instead of stamped onto every pod, plus a scheduler that
refuses to bind a claim to a node that lacks the volume. But local PVs have no dynamic
provisioner — we would write a PV object per workspace at create time, i.e. exactly the same
amount of generated YAML, plus a PVC, plus a binding lifecycle to get wrong on delete. The
affinity we would be buying is affinity we already know statically. Revisit if we ever want
the scheduler to *choose* the node based on free pool capacity; today the admission path
chooses, and the answer is already written on the `Volume`.

## Networking: flannel, CoreDNS, NetworkPolicy

k3s defaults do all of it.

- **flannel VXLAN** for pod-to-pod across nodes. The session↔env edge — a workspace reaching
  its team's environment — is now just pod networking. The replaced design closed that edge by
  *placement* ("a workspace that attaches to an environment is scheduled onto that
  environment's node") and had to forbid attaching to two environments to keep it true. That
  constraint is gone. A session-node workspace talks to an env-node service through flannel.
- **CoreDNS** for service names. `db` inside `env-{id}`; `db.env-{id}` from a workspace
  namespace.
- **NetworkPolicy** via kube-router (shipped and enabled in k3s). Every namespace gets a
  **default-deny ingress** policy at creation. An environment's own services get an allow for
  same-namespace traffic. A workspace attaching to an environment gets one explicit
  `NetworkPolicy` in the environment's namespace allowing ingress from the workspace's
  namespace, keyed by namespace label.

The attach rule from the replaced design survives as an **authorization** rule, because it
always was one: joining an environment's network reaches every service in it, so
`MembershipCheck` runs at attach time, not only at environment creation. What does *not*
survive is the placement coupling and the one-environment limit — a workspace may attach to
several environments now, each attachment being one more NetworkPolicy.

Also note the `/16`-per-environment exhaustion the replaced design found live (four
environments holding `172.18-21/16`, a hard ceiling near 16 environments per node). Under k3s
every pod is on the cluster pod CIDR; there is no per-environment subnet to allocate and
nothing to exhaust. The bug does not get fixed, it stops existing.

## Two writable copies of one fact — and the winner

The earlier framing in this design ("Cosmos is authoritative, k8s objects are derived") does not
survive contact with CRDs, and pretending otherwise would leave exactly the two-writable-copies
problem it was trying to avoid. With CRDs the Kubernetes API **is** the reconcile substrate, so:

> **The CRD spec is the single source of truth for the lifecycle of a workspace, environment,
> and volume. `bins/api` (`crates/workspaces/src/api.rs`) writes CRDs, controllers reconcile
> them, status flows back to the CRD status subresource. There is no second writable copy.**

Cosmos is not retired, but it is **reduced to two roles, neither of which overlaps the CRDs**:

1. **`Region` remains in Cosmos**, and only there. A region names an Azure storage account,
   blob container, and (today) an agent token. It is *cross-cluster* metadata — one k3s cluster
   per region is the whole point — so it cannot live in any one cluster's API server. The `/v1`
   API reads it to decide which cluster to talk to.
2. **A read-optimized projection**, if and only if we need cross-region listing ("all of
   alice's workspaces, everywhere"). The k8s API server is a poor list-and-filter store across
   clusters. If we build this, it is **write-only from the controller's status path and never
   read back into a decision** — a cache with a stated staleness, rebuildable by listing the
   CRDs.

**The rule when they disagree: the CRD wins, always, without exception.** The projection is
repaired from the CRDs; the CRDs are never repaired from the projection. If that rule is ever
inconvenient, the correct response is to delete the projection, not to soften the rule. A
projection that can write back is a second control plane, and we already know how that ends —
the compose file on disk that the design this replaces spent three bullet points describing.

`Snapshot` and `CommitRecord` are unaffected and are neither: they live in the registry
(`vol/{owner}/{id}` in `bins/server/src/vol_agent.rs`), which is content-addressed, immutable,
and already the durable record. Nothing about push history moves into k8s. A CRD holds a
*pointer* to the tip (`status.lastPush`), never the history.

### Drift repair

Standard controller semantics, with one keep-biased rule added:

- Every reconcile is level-triggered: read the spec, observe reality, converge. A Deployment
  someone deleted by hand is recreated on the next resync (default 10 minutes, plus immediate on
  any watch event). This is the "standalone controller we'd add later" from the pre-CRD draft —
  it is now free.
- Child objects (Namespace, Deployment, Service, NetworkPolicy) carry an `ownerReference` to
  their CRD, so **deletion cascades via garbage collection**. `EnvDelete`'s container half stops
  being code we write.
- The btrfs half cannot cascade — the GC does not know about subvolumes — so `Volume` carries a
  **finalizer** (`kloudlite-git.io/subvolume`). Deleting a `Volume` blocks until the node controller
  has run `cleanup_local` and removed the finalizer. That is the correct ordering guarantee for
  free: containers gone (GC), then subvolume gone (finalizer), then the object disappears.
- **Keep-biased**: a reconcile that cannot read what it needs deletes nothing. An API error, an
  unreadable pool, a node that just booted with the disk not yet mounted — all mean "requeue with
  backoff", never "reality doesn't match, so remove it". Same discipline as
  `crates/registry/src/gc.rs`, and the difference between a reconciler and a wrecking ball.

## Long-running btrfs work in a reconcile loop

A multi-GB `btrfs send` plus an Azure upload takes minutes. A reconcile must return in seconds.
The two are reconciled the way every controller handles slow work: **reconcile starts it, returns
immediately, and observes it on a later pass.**

The node controller keeps an in-process map of running operations, keyed by `{volume uid,
generation}`. One reconcile of a `Volume` whose `spec.generation` demands work not yet reflected
in `status.observedGeneration`:

1. If an operation for this key is already running → set `Progressing`, `RequeueAfter(15s)`,
   return. **This is the idempotency guard**, and it is a local in-memory check rather than a
   distributed lease, because the field selector already guarantees this node is the only
   reconciler of this object.
2. If none is running → spawn it on `spawn_blocking` (exactly as `run_job_blocking` does today,
   `bins/agent/src/lib.rs:310`, and for the same reason: `engine::ws_lock`'s synchronous
   `libc::flock` must not sit on the reactor), record it in the map, set condition
   `Progressing=True` with a reason, `RequeueAfter(15s)`, return.
3. On a later pass, a finished operation → write the outcome into status
   (`observedGeneration`, `lastPush`, `lineageTip`, `Ready=True` or `Ready=False` with the
   error), remove it from the map, return with no requeue.

Progress that matters to a user (bytes sent, upload percent) is written to status periodically by
the operation itself, not by the reconcile — status writes are cheap and the API server is the
right place for a progress bar to read from.

**What replaces the H2 lease-renewal problem.** Audit H2 is: a fixed 120s lease with no renewal,
against operations that take minutes, so the sweep requeues a still-running job and it runs
concurrently with itself, and non-idempotent replays (`create_subvol` on an existing `live`,
`ops.rs:166`; clone into an existing path, `ops.rs:624`) then mark a healthy workspace `Error`.
Three things replace it, and all three matter:

- **No lease at all.** `spec.nodeName` assigns; nothing expires; there is nothing to renew.
- **In-process single-flight** (step 1) makes double-start impossible on the happy path.
- **Idempotent reconcile is still required**, because the pod can restart mid-`send` and lose
  its map. This is the one obligation the CRD model does not remove: `create_subvol` and the
  clone paths must tolerate an existing `live`, and a crashed push must be resumable — which the
  staged-push crash-recovery seam (`unpushed` in `LineageEntry`, `model.rs:113`) already does,
  and is the single best-built thing in the current engine. **Fix the `create_subvol`/clone
  tolerance as part of this work**; it is the half of H2 that survives.

A pod restart mid-operation therefore looks like: pod comes back, watches, sees a `Volume` with
`observedGeneration < generation` and a stale `Progressing`, restarts the operation from scratch,
and the engine's own idempotency absorbs the replay. That is a worse outcome than resuming, and
it is acceptable — it is a retry, not a corruption.

## What happens to the job machinery

The bespoke queue was an orchestrator we wrote because we had none. We have one now.

| Type | Fate | Replaced by |
|---|---|---|
| `Job` (`model.rs:302`) | **deleted** | the CRD itself: the object *is* the work item |
| `JobKind` (`model.rs:260`) | **deleted** as a dispatch enum | a spec field (`desiredState`, `source`) that reconcile converges toward. There is no `WsStart` "job" — there is a spec that says Running. |
| `JobState` (`model.rs:294`) | **deleted** | `status.conditions` (`Ready`, `Progressing`, `Degraded`) — standard, `kubectl`-readable, and multi-dimensional in a way a four-value enum is not |
| `attempts` / `RETRIES` | **deleted** | controller-runtime's exponential requeue-with-backoff, which never gives up but backs off — strictly better than "3 attempts then permanently Error" |
| `lease.rs` (lease expiry + requeue sweep) | **deleted entirely** | `spec.nodeName`. Audit H1, H2, H4, H7 are all defects *in this file and its callers* and all cease to exist with it. |
| `spawn_sweep` (`vol_agent.rs:607`) | **deleted** | resync interval. Also retires P6 (the sweep running unguarded on every replica). |
| `/vol-agent/register`, `/work`, `/jobs/{id}/done`, `/jobs/{id}/failed` (`vol_agent.rs:628`) | **deleted** | watch + status subresource |
| `AgentDoc` (`model.rs:28`), heartbeats, `Capacity.used` | **deleted** | node objects and kubelet-reported allocatable. Retires P5 and P7. |
| `App::announce_stranded_merges`-style re-announce beat for volume jobs | **deleted** | level-triggered reconcile *is* the re-announce, on every resync |
| `scheduler.rs` `Binding` | **survives, narrowed** | see below |
| `Region` (`model.rs:15`) | **survives in Cosmos** | cross-cluster metadata; see the source-of-truth rule |
| `Snapshot`, `LineageEntry`, `CommitRecord` | **survive untouched** | the registry, unchanged |
| `Workspace`, `Environment`, `Service`, `Mount`, `WsState`, `EnvState` | **survive as CRD types** | same structs, `state` becomes `spec.desiredState` + `status.phase` |

**Note the two beats in `bins/server/src/lanes.rs` and the git-side merge job system are NOT in
scope.** `crates/pulls`' merge jobs run in `kloudlite-git-worker` against SlateDB and have nothing
to do with volumes. This section deletes the *volume* job system only.

### Who decides placement

**The `/v1` admission path decides, once, at create time, and writes `spec.nodeName` into the
CRD.** The k8s scheduler does not choose the node for a workspace, because it cannot know where
the subvolume is.

The rule "all of one owner's workspaces on one node" is expressed exactly as it is today —
`Binding` (`model.rs:47`), an owner→node record — but it moves out of Cosmos and becomes a
lookup the admission path performs before writing the CRD:

- First workspace for an owner in a region: pick the node. The pick uses **node allocatable
  minus requests** (real numbers from the k8s API) instead of `free()`'s
  `capacity - used` guess over a flat `JOB_CPU`/`JOB_MEM_MB` estimate
  (`scheduler.rs:35`, `bins/agent/src/lib.rs:146` — already marked `ponytail:` as not real
  accounting). Persist the binding.
- Every later object for that owner: read the binding, write that `nodeName`. `bind_owner`'s
  conflict-adopt logic (`scheduler.rs:62`) carries over verbatim — it is correct and it stays
  correct under optimistic concurrency.
- The binding itself is best held as a **label on the CRD plus a small `OwnerBinding` CRD**, so
  it lives in the same store as the thing it constrains and gets the same optimistic-concurrency
  guarantee (`resourceVersion` conflict → re-read and retry, exactly where `StoreErr::CasFailed`
  → re-read sits today). Keeping it in Cosmos would recreate the two-stores problem for the one
  fact that decides where data goes.
- The pod's `nodeName` comes from the `Volume`'s, never independently. A `Workspace` whose
  `spec.nodeName` disagrees with its `volumeRef`'s is rejected by the admission webhook — the
  one invariant that, violated, splits an owner's data across pools (which is precisely what
  audit H1 does today).
- **The scheduler's dead-node behaviour survives unchanged and deliberately:** a bound node that
  is gone leaves work pending rather than re-homing, because re-homing an owner is a migration
  of their subvolumes, not a scheduling decision (`scheduler.rs:91`'s comment says exactly this
  and it is right). Under k8s this surfaces as a pod stuck `Pending` on unsatisfiable affinity —
  visible, which is an improvement on a job silently sitting `Queued` forever.

What the k8s scheduler *does* decide: nothing, for workspaces and environment services. Every
one is pinned. That is not the scheduler being wasted — it is the scheduler being told the
truth about local storage.

## Images

`Service.image` and `Workspace.image` are pulled by the kubelet, not by us. Three notes:

- **`imagePullPolicy: IfNotPresent`** for tagged images, which is the k8s default for anything
  not tagged `:latest`. `default_ws_image()` returns `nginx:alpine`, so the common workspace
  never pulls twice on a node.
- **The registry we already run is an obvious source.** `img/{owner}/{name}` in this repo's own
  OCI registry is a valid pull target; a per-namespace `imagePullSecrets` holding the owner's
  credentials makes `ttl.example/img/alice/devbox:v1` work. That is a small addition and it is
  the first thing users will ask for.
- **Pre-pull the default image onto both worker nodes** at provisioning. A cold `nginx:alpine`
  pull is the difference between a workspace that is ready in three seconds and one that looks
  hung.

## Resource requests and limits

Node sizing forces the issue: 32 OCPU / 128 GB on the session node, running user-chosen images.

Set both a request and a limit on every workspace and service container. Requests are what the
scheduler packs against; limits are what stops one workspace eating the node.

- **Workspace default**: request `500m` / `1Gi`, limit `4` / `8Gi`. Twelve concurrent
  workspaces fit comfortably in requests; a busy one can burst to 4 cores. Make the pair a
  per-workspace field with these as defaults — a compile job needs a different shape than an
  editor.
- **Environment service default**: request `250m` / `512Mi`, limit `2` / `4Gi`. Services are
  databases and sidecars, mostly idle.
- **ResourceQuota per namespace** so one owner cannot create fifty services. A LimitRange
  supplies the default to any container that omits one, which closes the "user submits a spec
  with no limits" hole without validating it ourselves.
- **CPU limits throttle rather than kill; memory limits OOM-kill.** A workspace that
  OOM-kills is a Deployment that restarts it — data on the subvolume is fine, in-memory work is
  not. Surface the restart to the user; a silently restarting workspace is worse than a stopped
  one.

`Workspace.quota_gb` (`model.rs:91`) remains what the audit says it is: accepted, stored, and
enforced nowhere, because disk is btrfs and k8s has no say. Enforcing it means a btrfs qgroup
on the subvolume, in `engine::ops.rs`, unrelated to this design. Not doing it here — but note
that with 128 GB nodes and shared pools, "a 5 GB workspace can fill the pool" is a real
availability bug and belongs on the list.

## Multi-tenancy, honestly

**A namespace is not a security boundary.** It is a name scope with RBAC and NetworkPolicy
attached. Every pod on the session node shares one Linux kernel with every other tenant's pod.
A container escape — a kernel LPE, a runc bug — reaches every workspace on that node. This is
exactly as true under k3s as it was under docker; the migration neither improves nor worsens it.

What we do get, and should actually use:

- `automountServiceAccountToken: false` on every user pod. A user workspace with a mounted
  token can talk to the API server, which is a real escalation path we would be creating for
  free.
- A restrictive `securityContext` on user pods: `allowPrivilegeEscalation: false`, drop `ALL`
  capabilities, `readOnlyRootFilesystem` where the image tolerates it, non-root where possible.
  Enforce with the Pod Security Admission `restricted` profile labelled on `ws-*` and `env-*`
  namespaces — which is a label, not code we write. The agent's own namespace is `privileged`
  and is the only one.
- Default-deny NetworkPolicy per namespace, as above. Without it, every tenant's pods can reach
  every other tenant's pods and the cluster's internal services.

What we do **not** get and should not claim: hard tenant isolation. The answer to that, when it
becomes a requirement, is a sandboxed runtime dropped in as a `RuntimeClass` — **gVisor**
(`runsc`, syscall interposition in userspace, cheap, breaks some workloads) or **Kata**
(per-pod lightweight VM, real boundary, slower start, and it complicates the hostPath mount we
depend on). Both are per-pod opt-in via `runtimeClassName`, so the upgrade path is a field on
the pod spec and a node-level install — no architectural change. Record it; do not build it.

This is the same conclusion the replaced design reached ("isolation is a separate decision from
weight"), and it survives the change of orchestrator intact, because it was never about the
orchestrator.

## Migration

Two environments are live on the production docker VM (`demo-env`, `mongo-test`), plus whatever
workspaces exist.

The migration is easier than it looks for one reason: **an environment's data is in its btrfs
subvolume and its pushed history, never in its containers.** Recreating containers is not data
loss. Bringing an environment up already implies a restart.

1. **Stand up the k3s cluster alongside the running VM.** Control plane, both workers, both
   worker nodes' btrfs pools created and labelled. Nothing migrated yet.
2. **Push everything first.** For each live environment and workspace, trigger an explicit
   `push`. This is the safety net: after this step every byte that matters exists in Azure blob
   storage under `blobs/{owner}/{algo}/{hex}`, addressed by content, reachable from any node.
   Do not proceed on anything whose push failed.
3. **Migrate the pools.** For a same-machine cutover (the k3s worker *is* the old VM,
   re-provisioned) the pool is already there and the node controller finds it under the same
   `$NODE_NAME`. For a new machine, the volume comes down from the registry:
   `pull_env`/the restore path already does exactly this, which is what H3 was about. **Fix H3
   before migrating to new hardware** — the re-materialization path is currently dead code with
   an argument-swapped call site (`bins/agent/src/lib.rs:516`). If we do a same-machine cutover
   we can migrate without it, but then we are shipping a cluster that cannot rebuild a node.
4. **Write the CRDs.** One `Volume` + one `Environment` (or `Workspace`) object per live
   resource, translated from its Cosmos document by a one-shot migration script, with
   `spec.nodeName` set from the existing `Binding`. The controllers do the rest: namespace,
   Deployments, Services, policies. Verify service-to-service DNS and data presence before
   touching the docker VM. Keep the script — it is also how a second region gets bootstrapped.
5. **Tear down docker last**, and remove the leaked `{pool}/env/{id}` compose directories
   (audit M1) as part of it — under k3s nothing writes them again, so this is a one-time sweep
   rather than the ongoing cleanup the replaced design had to add.

No dual-run compatibility shim. The replaced design needed one ("match either `kloudlite-git.id`
or `com.docker.compose.project` for one release") because it was replacing docker *with docker*.
Here the two worlds do not share a node, so the cutover is per-environment and reversible by
pointing the old VM's agent back at the queue — **as long as the Cosmos job documents are left
intact until the cutover is signed off.** Delete them in a separate, later commit; a rollback
that has nothing to roll back to is not a rollback.

## The e2e test

`tests/ws_e2e.sh` today spins server + api + agent + Cosmos + Azure + btrfs and exits 77 when a
prerequisite is missing. Its shape survives; four things change:

- **The prerequisite becomes a k3s cluster** (a single-node k3s in the CI VM is enough — one
  node with both role labels, taints relaxed). Add it to the 77-skip check alongside
  root-capable btrfs. Nothing about the test's value depends on there being two nodes.
- **Container assertions move from `docker` to `kubectl`.** `sudo docker exec env-{id}-db-1
  mongosh ...` becomes `kubectl -n env-{id} exec deploy/db -- mongosh ...`. Every runbook
  command in the docs needs the same edit; grep for `docker exec` and `docker compose`.
- **Waiting changes shape, for the better.** Today the script polls a document until its state
  flips. Now it is `kubectl wait --for=condition=Ready workspace/{id} --timeout=…` — the
  condition the controller writes. That is a strictly better test: it asserts the *contract*
  (`Ready` means ready) rather than a state string, and it fails with the condition's own
  message instead of a timeout with no reason.
- **Three assertions worth adding, because they are what this design claims:**
  service-to-service DNS resolves across namespaces (a workspace pod resolving `db.env-{id}`),
  and a default-deny namespace actually denies (a pod in an unattached namespace fails to
  reach it). A NetworkPolicy nobody tests is a NetworkPolicy that is silently not enforced —
  kube-router not being enabled is a config typo away and produces no error, only permitted
  traffic. Third: **reconcile converges** — delete a Deployment out from under a running
  environment and assert it comes back without anyone calling the API. That is the whole claim
  of moving to controllers, and nothing else in the suite tests it.

The btrfs half of the test — push, clone, restore, the MongoDB clone-fidelity check — is
unchanged, because that engine is unchanged. That is the useful signal that this migration is
scoped correctly: **the test's hardest assertions do not move.**

## What this deletes

Cancelled outright, from the replaced design (never built):

- The bollard direct-runtime module (`bins/agent/src/runtime/` — `mod.rs`, `spec.rs`,
  `reconcile.rs`, `net.rs`), ~250-300 lines of reconcile logic, and the bollard dependency.
- The label schema (`kloudlite-git.owner`/`.kind`/`.id`/`.service`/`.spec`) and spec-hashing —
  k8s labels, ownerReferences and the Deployment's own generation do this.
- Per-environment bridge network creation, network aliases, and connect/disconnect.
- **The per-node IPAM block allocator**: fixed node blocks, `/26` per environment,
  `default_subnet_pools` pinning, host-route overlap checks. Flannel does this.
- The Podman-vs-Docker socket abstraction, API version pinning, `crun` runtime selection, and
  the podman.socket systemd unit.
- The "cross-machine: deliberately none" placement contortion, and with it the
  **one-environment-per-workspace attachment limit** it forced.
- The WireGuard/VXLAN/overlay/route-distribution work that section was deferring — not deferred
  now, deleted.

Deleted from the current codebase:

- `crates/workspaces/src/engine/compose.rs` (124 lines) — YAML rendering, `up`, `down`, the
  compose file on the pool disk, and `{pool}/env/{id}` entirely. `render`'s mount-validation
  test moves to whatever builds the pod spec; **do not lose it** — it is the C1 regression test.
- `bins/agent/src/container.rs` (102 lines) — `docker run/start/stop/rm/inspect` shell-outs and
  the `{{.State.Running}}` stdout parsing at `container.rs:77`.
- `docker_stop_name`/`docker_start_name`/`compose` helpers in `bins/agent/src/lib.rs:693-734`,
  and the `stop_projects`/`stop_container`/`stop_project` payload plumbing around them once the
  stop/start hooks become replica scaling.
- The `serde_yaml` dependency (archived, RUSTSEC-2024-0320 — audit finding 15) if compose.rs was
  its only user. Check before claiming it; the swap-to-`serde_yml` task may become moot.
- The `used_cpu`/`used_mem_mb`/`used_disk_gb` query params and the flat `JOB_CPU`/`JOB_MEM_MB`
  accounting (`bins/agent/src/lib.rs:146`), replaced by node allocatable.

Deleted because the Kubernetes API replaces it (full table in "What happens to the job
machinery"):

- **The entire volume job system**: `Job`/`JobKind`/`JobState` (`model.rs:260-312`),
  `crates/workspaces/src/lease.rs`, `spawn_sweep` (`vol_agent.rs:607`), and the four job routes
  `register`/`work`/`jobs/{id}/done`/`jobs/{id}/failed` (`vol_agent.rs:628`).
- The agent's long-poll loop `run_with_engine` and its `report`/`run_job` dispatch
  (`bins/agent/src/lib.rs:160-218,318-548`), replaced by watch + reconcile.
- `AgentDoc` (`model.rs:28`), agent registration, `{pool}/agent-id`, and the heartbeat write.
- **Four audit findings cease to be expressible** rather than being fixed: H1 (leasing a job
  with `agent: None` onto the wrong node), H4 (stale completion reports overwriting job state),
  H7 (sweep-exhausted jobs never marking the doc Error), P6 (the sweep running unguarded on
  every replica). H2 is half-deleted — the lease half goes, the "make replay idempotent" half
  is real work this design explicitly takes on.

Not deleted, deliberately: `crates/workspaces/src/scheduler.rs`'s binding logic (narrowed to a
data-locality lookup, moved out of Cosmos), the registry-side volume-commit routes
(`vol_agent.rs`'s `commits`/`ref`/`history`), `Region` in Cosmos, and every line of
`engine/ops.rs`, `engine/blob.rs`, `engine/pool.rs`.

## What this costs

Stated plainly, because a design that only lists wins is selling something:

- **A control plane we do not currently operate.** Three nodes instead of one. k3s upgrades,
  certificate rotation (k3s auto-rotates but only if the server restarts within the validity
  window — a server up for a year hits expired certs), and a SQLite file that must be backed up.
- **A new failure mode class**: pods `Pending` for reasons that are not obvious (taint
  mismatch, unsatisfiable nodeAffinity because `spec.nodeName` names a node that no longer exists,
  insufficient allocatable). Today "it didn't start" has one place to look; under k8s it has
  five. The runbook needs a `kubectl describe pod` triage section on day one.
- **Debuggability changes shape.** `docker compose logs` and `docker ps` are gone; `kubectl
  logs` and `kubectl get pods -n env-{id}` replace them and are strictly better, but every
  runbook, every operator's muscle memory, and `tests/ws_e2e.sh` need rewriting.
- **Latency on the create path.** A Deployment → ReplicaSet → Pod → kubelet → image pull chain
  is slower than `docker run`. Pre-pulled images make it seconds; it is still slower.
- **CRDs are a schema we now own and must version.** An additive field is free; a rename or a
  semantic change to `spec` needs a conversion webhook or a v1beta1→v1 migration. Start at
  `v1alpha1`, say out loud that it is unstable, and do not promote it until the shape has
  survived a quarter.
- **Controller correctness has its own failure literature.** A reconcile that writes spec, a
  status write that triggers its own watch event, a hot loop from an unconditional requeue — all
  are easy to write and all are outages. The `/status` subresource RBAC split above prevents the
  first; the other two need a "does reconcile actually converge" test.
- **YAML we generate is a new correctness surface.** A pod spec with a wrong hostPath is a
  worse outcome than a compose file with a wrong bind, because the API server accepts it
  cheerfully. `validate_mount` runs on the way in, as it does today, and the hostPath source is
  built only from validated segments — never from a caller-supplied path. That rule from the
  replaced design carries over verbatim and is the one thing in this document that must not be
  relaxed.

## Not doing

- **HA control plane / etcd.** Single server, SQLite, accepted ceiling with a stated blast
  radius. Revisit when control-plane downtime actually costs something.
- **A cluster-wide controller, or leader election.** Sharding by `spec.nodeName` gives one
  candidate per shard. Add `coordination.k8s.io` Leases when a genuinely cluster-scoped
  controller exists, not before.
- **A conversion webhook.** `v1alpha1` only, additive changes only, until the shape settles.
- **A cross-region projection store.** Build it when a cross-region listing is actually asked
  for, and build it write-only (see the source-of-truth rule).
- **Local PersistentVolumes.** hostPath + nodeAffinity, argued above.
- **gVisor/Kata.** Recorded as the runtimeClass upgrade path, not built.
- **Autoscaling, cluster-autoscaler, HPA.** Two fixed nodes.
- **Migrating the git/registry tier into this cluster.** Unrelated, separately deployed, out of
  scope.
- **Fixing audit H5** (a deleted workspace resurrected to Ready by an in-flight job) as a
  patch. Finalizer-ordered deletion makes it structurally impossible, so the patch would be
  dead code the day it merged. H1/H4/H7 are the same story — see "What this deletes".
- **Fixing H3** (dead environment re-materialization) as part of *this* design — but it is a
  hard prerequisite for migrating onto new hardware, and for node rebuild in general. Do it
  first, separately.
- **Making a crashed reconcile resume mid-`send` rather than restart it.** A retry is correct,
  just slower. Resumable uploads are a real feature; they are not this feature.
