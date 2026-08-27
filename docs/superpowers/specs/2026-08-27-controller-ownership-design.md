# Controllers own their children — design

Status: proposed, 27 Aug 2026. Supersedes the placement and child-creation parts of
`2026-08-26-k3s-architecture-design.md`; everything else in that document stands.

## The problem, in one paragraph

Today `/v1` (the API tier) does controller work at request time: it picks the node, creates the
`Volume` and the `Workspace`/`Environment`, and drops the owner's SSH key into a namespace that
does not exist yet. The controllers then reconcile objects they did not create, do not wait for
each other (a `Workspace` makes its pod before its `Volume` has made the disk), and re-ensure
shared per-owner objects on every pass. The git-seeding path was never wired end to end: the API
named a token Secret nobody writes and the agent has no permission to read one. The user-visible
symptom on 27 Aug: "Open in a workspace" produced a pod stuck on `path … does not exist` forever.

## The rule

**The API writes what the user wants. Controllers make it happen, and own every object they make.**

- The API creates exactly one object per user action: a `Workspace` or an `Environment`, unplaced.
  It never names a node, never creates a `Volume`, never writes into a workspace namespace
  before that namespace exists.
- A controller that creates a child stamps an `ownerReference`; the child dies with the parent.
  A controller never edits the *user's* fields of an object it does not own; authoring its own
  children's spec is what a controller is for (a Deployment writes ReplicaSet spec; nobody calls
  that a violation).
- Status flows up: a parent acts on a child only by reading the child's `status`, never by
  guessing.
- **The cluster is the source of truth** for `Workspace`, `Environment`, `Volume`,
  `SnapshotRequest` and `OwnerBinding`. There is no copy in the API, none in Cosmos (which holds
  only `Region`), none in the web. Every `/v1` read is a projection of a CR. The only state
  outside the cluster is the snapshot data and its commit record on the server tier — bytes
  and cross-region history that cannot live in etcd.

## Objects

All five kinds stay cluster-scoped and keep `/status`. `Workspace` and `Environment` select on
`.status.nodeName` (a status path is a legal selectable field — only metadata is forbidden and
arrays are not allowed, per the API server's own `kubectl explain` on k3s v1.33; and "if jsonPath
refers to an absent field in a resource, the jsonPath evaluates to an empty string", so
`status.nodeName=` matches an object that has no status yet). `Volume` and `OwnerBinding` keep
`.spec.nodeName` (their spec is controller-written); `SnapshotRequest` has none.

`phase` is a Rust enum on every kind so the generated schema carries `enum`. Every status carries
`observedGeneration` and `conditions`; the no-op guards that ignore `lastTransitionTime` stay.

### `Workspace` (API-written)

`spec`: `owner`, `team`, `name`, `region`, `image`, `resources`, `desiredState`, and new:

- `storage: { quotaGb, source? }` — what used to be the `VolumeSpec` the API authored.
  `source` is `cloneOf { workspace }` | `restoreOf { workspace, snapshotId }` |
  `gitRepo { repo, branch }` — the same three, minus `credential_secret`, which is deleted.
- `nodeName` — **removed from spec.** Placement is a fact the controllers establish, so it
  lives in status. Controllers never write a Workspace's spec.
- `volumeRef` — **removed from spec** (it was a wish about a fact). The child is found by
  ownerReference and reported in `status.volumeRef`.

`status`: `phase` (`pending` | `creating` | `ready` | `stopped` | `error`), `observedGeneration`,
`nodeName` (where it runs now; selectable; written by the claim), `compatibleNodes: [string]`
(every node that holds this object's volume — the memory placement uses when `nodeName` is
empty), `volumeRef`, `podRef`, `conditions` (`Placed`, `VolumeReady`, `Ready`).

### `Environment` (API-written)

Same shape: gains `storage`, loses `volumeRef` and `nodeName` from spec, gains
`status.nodeName` + `status.compatibleNodes`.

### `Volume` (controller-written child)

Unchanged spec except `source.gitRepo` loses `credential_secret`, and `spec.team` is retained
(the namespace name needs it). Created by the parent's reconciler with
`ownerReferences: [parent]`. Keeps its finalizer and its four-state work loop exactly as today —
this controller is the one that already works.

### `SnapshotRequest` (API-written, new)

A push is a user wish with an outcome, so it is a CR, not an annotation. The CR is the
**request**; the **snapshot record** is what its reconciler writes to the server tier.

`spec: { volume, message? }` — `volume` names the `Volume`. Nothing else: a node is a
controller-owned fact and the API does not copy facts into spec. Every agent watches all
`SnapshotRequest`s (no field selector — two nodes today) and acts only when the named Volume's
`spec.nodeName` is its own. `status: { phase: pending|working|done|error, observedGeneration,
snapshotId, lineageTip, at, conditions: [Progressing, Ready] }`.

**Finalizer** `rustic-git.io/snapshot`: a delete while `working` must wait for the in-flight
btrfs send / upload to finish (the same reason `Volume` has one) — a request is removed only
when nothing is running for it.

Cluster-scoped, labelled `rustic-git.io/owner` and `rustic-git.io/volume`. **Not owned by the
Volume**: a snapshot outlives a deleted workspace, because the thing it names still exists.

The record: the registry commit record (`vol/{owner}/{id}` on the server tier, written by the
reconciler via `POST /vol-agent/{owner}/{id}/commits`) is the snapshot itself — durable,
content-addressed — it is cross-region and it is what a cold clone or a restore on
another node reads. The `SnapshotRequest` CR carries the wish and, in `status`, the outcome: the record's id.
Deleting the CR deletes no data. `ponytail:` no snapshot deletion or retention yet; the GC sweep
for blobs is unchanged.

Replaces: the `push-requested` / `push-message` annotations on `Volume`, and
`Volume.status.lastPush`, which is **dropped**: "the latest snapshot" is a query over
`SnapshotRequest`s by volume label, not a second writer of the Volume's status (two controllers
force-applying one status object under one field manager prune each other's fields).

### `OwnerBinding` (controller-written)

`spec: { owner, region, nodeName }` — created by the claiming agent, atomically (`create` on the
fixed name `{region}-{owner}`; a second creator gets 409 and re-reads). Gains a reconciler that
owns the **per-owner shared objects** on that node's namespace: the namespace itself
(`ws-{team}-{owner}` per team the owner works in is a Workspace-time concern — see below), the
`LimitRange`, default `NetworkPolicy`s, and the `RoleBinding` that lets the API write Secrets
there. `status: { conditions: [NamespaceReady] }`.

Namespace per (team, owner) pair: the `OwnerBinding` reconciler ensures one namespace per team
the owner has a placed Workspace in. Concretely it ensures `ws_namespace(owner, team)` for every
distinct `team` among that owner's Workspaces on its node, plus the personal one. The Workspace
reconciler waits on `NamespaceReady` for its own namespace rather than creating it.

## Placement

A `Workspace` with empty `status.nodeName` is *unplaced*. Each agent runs a second `Workspace`
watch with field selector `status.nodeName=` (empty) — only agents on `rustic-git.io/session`
nodes do this for Workspaces; only agents on `rustic-git.io/env` nodes for Environments.

Claim, in the reconciler of that watch (this node = `me`):

1. `status.compatibleNodes` non-empty and `me` not in it → do nothing; a listed node claims.
   (A `cloneOf` source: the new object's `compatibleNodes` is empty, so the rule is instead
   "`me` is in the SOURCE's `compatibleNodes`" — a local clone needs the source's disk.)
2. Otherwise claim = one **optimistic** status write: `replace_status` (or a non-forced
   server-side apply) carrying the object's current `metadata.resourceVersion`, setting
   `status.nodeName = me`, `status.compatibleNodes = union(existing, {me})`, condition
   `Placed`. A 409 means another node won; re-read and go to 1. A forced apply would never
   conflict and is therefore wrong here — this is the one write in the system that must race.
3. Ensure the `OwnerBinding` `{region, owner}` → `me` exists (atomic `create`; 409 is fine) so
   the per-owner namespace reconciler runs here.

The claim is a **status** write: controllers never touch an API-authored spec.

`compatibleNodes` is written for the future, not just for restart: when snapshots are
replicated across nodes for high availability (later design), every node that has received a
replica appends itself, and a workspace whose home node is down can start on any listed node.
Nothing in this design writes more than one entry; nothing in it should assume there is only one.

Stop keeps `status.nodeName` — the disk has not moved, so a later start reconciles on the same
node with no placement step. `nodeName` is cleared only by a node-retirement path (later
design), at which point `compatibleNodes` decides where it may start again.

Scope, decided 27 Aug: **two nodes for now — one session node, one env node.** The claim
logic above is written so a second node of a role needs no redesign, but multi-node placement
(capacity, spreading, re-homing an owner) is explicitly a later design. Until then:
`ponytail:` the claim checks no free space, and the placement algorithm moves into the agent
unchanged (`placement.rs`), so a second session node is a deploy, not a code change.

Init container image for git seeding: pinned `alpine/git` (so seeding works with any workspace
image); the pin lives in the agent's env like `WS_GIT_SSH_HOST`.

`audit H1` invariant ("a Workspace never names a node its Volume does not") holds by
construction: the Volume is created by the Workspace's reconciler *from* `status.nodeName`, and
`volume_node`'s mismatch guard stays as the belt to that brace.

## What triggers what

A reconciler runs on (1) a change to its watched CR — any field, annotations and status
included; (2) a change to a child it watches, mapped back to the parent; (3) a timer it asked
for. Every dependency below is wired as (1) or (2); `requeue` is only ever the backstop.

| Reconciler | Watches (primary) | Also watches → mapped to primary by | Woken by |
|---|---|---|---|
| Placement (per agent, its role) | `Workspace`/`Environment` with `status.nodeName=` (empty) | — | create of an unplaced object, or a cleared `nodeName` |
| Workspace | `Workspace` with `status.nodeName={node}` | `Volume` → ownerReference; `Pod` → ownerReference (label-selected to `rustic-git.io/kind=workspace`); `OwnerBinding` → `spec.owner` == binding owner; `Workspace` → `storage.source.cloneOf.workspace` (a clone waits on its source's `compatibleNodes`) | claim write, Volume status (`ready`), pod readiness, `NamespaceReady` |
| Environment | `Environment` with `status.nodeName={node}` | `Volume` → ownerReference; `Deployment` → ownerReference; `SnapshotRequest` → ownerReference (the stop snapshot is its child) | claim write, Volume status, deployment readiness, stop snapshot `done` |
| Volume | `Volume` with `spec.nodeName={node}` | — | creation by parent, finalizer on delete |
| SnapshotRequest | all `SnapshotRequest`s, acting only when the named Volume is on this node | `Volume` → `spec.volume` (a request created before its Volume is placed waits) | creation by the API (push) or by the Environment reconciler (stop); finalizer on delete |
| OwnerBinding | `OwnerBinding` with `spec.nodeName={node}` | `Workspace` → `spec.owner` (a new team namespace may be needed) | binding create, a Workspace of that owner appearing |

The claim is one status write and two watch events: the object leaves the unplaced selector and
enters the node's selector. No poll.

## Reconcile flows

### Workspace (agent on its node)

1. `heal_labels`.
2. Ensure child `Volume` exists (`ownerReference` → Workspace; spec from `storage` + placement).
   Do not proceed until `Volume.status.phase == ready` and `subvolumePresent`; write
   `phase: creating`, condition `VolumeReady=False`, requeue 15s.
3. Wait for `OwnerBinding` condition `NamespaceReady` for `ws_namespace(owner, team)`;
   requeue 15s until true.
4. Ensure `PersistentVolume` + `PersistentVolumeClaim` (owned by the Workspace — as today).
5. `desiredState`:
   - `running`: create the pod if absent. The pod carries an **init container** when
     `storage.source` is `gitRepo`:
     ```
     image: $WS_GIT_INIT_IMAGE   # pinned alpine/git from the agent env; any workspace image works
     command: sh -c 'set -e; [ "$(ls -A /workspace)" ] || git clone --depth 1 --single-branch --branch "$BRANCH" "$URL" /workspace'
     env: GIT_SSH_COMMAND (same value as the main container), URL=ssh://git@{WS_GIT_SSH_HOST}[:{port}]/{owner}/{repo}.git, BRANCH
     volumeMounts: live at /workspace, user-key at /etc/rustic-git/ssh (both as the main container)
     securityContext: hardened()   # same user; files land owned by the workspace user
     ```
     `WS_GIT_SSH_HOST` / `WS_GIT_SSH_PORT` come from the agent's env (DaemonSet), as
     `WS_GIT_BASE` does today; `WS_GIT_BASE` and the agent-side `git_clone` are deleted.
     The init container is idempotent (empty-dir check), so a pod restart never re-clones over
     a user's work.
   - `stopped`: delete the pod. The Volume, PV, PVC stay.
6. Status as today (`creating` until the pod is Ready, then `ready`; `stopped`), plus
   `volumeRef`, `nodeName`.

### Environment

Identical skeleton: Volume child → NamespaceReady (`env-{id}`, which stays owned by the
Environment as today, so no OwnerBinding dependency for envs) → PV/PVC → Deployments/Services;
stop-with-push unchanged.

### Volume

Minus `read_git_token`, `git_clone`, and the whole push branch of `volume_work` (moves to
SnapshotRequest). `gitRepo` materializes as an empty subvolume; the pod fills it. What remains:
materialize (empty / clone-local / restore), the finalizer, the four-state work loop.

### SnapshotRequest (new reconciler, agent on its node)

1. `phase: working`, condition `Progressing`. The `running` map (keyed by the request's uid)
   is the idempotency guard exactly as for Volume work; the Volume's `ws_lock` serialises it
   against a clone-running or a restore on the same disk.
2. `spawn_blocking(engine.push_env(owner, volume, message))` — unchanged engine code: RO btrfs
   snapshot of `live`, delta against the previous snapshot, stage file, blob upload, one
   `POST /vol-agent/{owner}/{id}/commits`, one ref move.
3. Done → `status: { phase: done, snapshotId, lineageTip, at, observedGeneration }`. Nothing
   is written on the Volume. Failure → `phase: error`, condition `Ready=False/OperationFailed`,
   requeue with backoff. A request is never re-run past `done`.
4. **Agent restart while `working`**: the `running` map is gone, so the request has a
   `Progressing` condition and no handle. It is NOT re-run — a second `engine.push` would take a
   fresh snapshot and register a second commit record. It is marked `phase: error`, condition
   `Ready=False/AgentRestarted`; the user pushes again. `ponytail:` resume from the engine's
   `unpushed` stage mark instead of failing, once the engine exposes "is this lineage entry
   already registered".
5. Delete: the finalizer waits for an in-flight operation (as `cleanup_volume` does), then
   removes the object. Nothing on disk or in the registry is reclaimed by it.
6. Errors are classified: a permanent one (unknown volume, volume on another node forever,
   invalid spec) writes the condition and `await_change()`; a transient one (registry 5xx,
   btrfs busy) requeues with backoff. The same rule applies to every reconciler in this design.

Environment stop: the Environment reconciler creates a `SnapshotRequest` child (`ownerReference` →
Environment) on the first stop pass, then waits for its `done` before deleting the deployments.
The `await_stop_push` annotation dance is deleted.

### OwnerBinding

New reconciler, watching `spec.nodeName={node}`: ensure namespaces (one per team in use plus
personal), `LimitRange`, default `NetworkPolicy`s, `api_secret_binding`. Sets `NamespaceReady`.
Objects it creates carry an ownerReference to the `OwnerBinding` (which is cluster-scoped, so
this is legal) **except the namespace and LimitRange**, which keep no owner as today — an owner's
quota ceiling must not vanish with a binding rewrite. `ponytail:` bindings are never deleted;
a node retirement path re-homes them later.

### Deletion

- Delete `Workspace` → GC removes the `Volume` (finalizer orders the subvolume reclaim after the
  pod is gone, exactly as now — the API's "Workspace first, then Volume" ordering becomes the
  API server's job, and the API's delete handler shrinks to one call).
- Delete `Environment` → same.

## The API after this

`POST /v1/workspaces`: validate, build one `Workspace` (`storage`; no node), create,
202 with the projection. `clone` / `restore` build a `Workspace` whose `storage.source` says so;
they no longer copy a node from the source — locality is the `OwnerBinding`'s job. `delete` is
one delete. `start`/`stop` unchanged. `push` creates a `SnapshotRequest` (`volume` from
`Workspace.status.volumeRef`, `nodeName` from that Volume; 409 "not ready yet" while `volumeRef`
is unset) and answers 202 with the SnapshotRequest projection. `restore` takes a snapshot id, checks a `done` `SnapshotRequest` carries it, and builds a
Workspace whose `storage.source` is `restoreOf { volume, snapshotId }`. No
registry read on the request path. `GET /v1/volumes/{id}/history` lists `SnapshotRequest`s by the
volume label, newest first; `/refs` is derived from the newest `done` one. The registry
`get_history` client call leaves `bins/api` entirely.

Platform key: after create, the API polls the Workspace for `conditions[Placed]` (up to 5 s,
then gives up and logs — the next `/v1/workspaces` list for that owner retries the install if
the Secret is absent). The Secret is written with the existing per-namespace grant. The key mount
in the pod becomes **required** for `gitRepo`-seeded workspaces (the init container cannot work
without it) and stays optional otherwise.

`placement.rs` moves from `crates/workspaces` (API) to `bins/agent` — same algorithm, new caller.
`OwnerBinding` create/read leaves the API's RBAC.

## RBAC

Agent: gains `create` on `volumes`, `snapshotrequests` (the Environment stop child) and
`ownerbindings` (children it authors); `get/list/watch/patch/update` + `/status` +
`/finalizers` on `snapshotrequests`; keeps `/status` on everything; keeps `patch` on `workspaces`/`environments` main resource ONLY for
`heal_labels` (labels are metadata, not spec). The claim is a status write, so the design-doc
statement "the agent cannot write spec" becomes true in practice. `ponytail:` a
ValidatingAdmissionPolicy that refuses a non-label main-resource patch from the agent SA is the
mechanical version.

API: loses `ownerbindings` and `volumes` `create`/`delete`; keeps `get/list` on volumes for
projections; gains `create/get/list/delete` on `snapshotrequests`. `all_crds()` and
`deploy/k3s/crds.yaml` gain the fifth kind.

## What this fixes

- The stuck pod (disk not ready): Workspace waits on `VolumeReady`.
- Git seeding: works, on a first workspace too, with no new secret.
- First-workspace-without-key: the API installs after `Placed`, retries on list.
- Dead code: token Secret path, `WS_GIT_BASE`, API-side placement, API-side Volume creation,
  duplicated per-owner ensures in two reconcilers.
- `OwnerBinding` gets a status and an owner.

## What it does not do

- No capacity-aware placement (one node per role).
- No node retirement / re-homing.
- No change to the registry surface, the engine, or Cosmos. Push/clone/restore keep their
  semantics; only their triggers move (annotation → `SnapshotRequest`).
- Environments keep their own namespace per env; only workspaces use the per-owner one.

## Migration

**Two-step schema change.** A CRD apply is cluster-wide and pruning is irreversible, while the
agents roll per node: dropping `spec.nodeName`/`spec.volumeRef` in the same release that
migrates them would lose the Volume pointer for any object an agent had not yet migrated.
Release 1: the fields stay in the schema as optional, the new `status` fields and `storage`
block are added, agents migrate. Release 2 (after every node has rolled and every Workspace
carries `status.volumeRef`): drop the two spec fields. There is no rollback across release 2;
release 1 can be rolled back (old agents ignore the new status fields).

Existing objects: `Workspace.spec.volumeRef`, `Volume` without ownerReference, and pushed
history that exists only in the registry. The migration also backfills one `SnapshotRequest` per
registry commit record for each Volume on this node (`phase: done`, ids from the record), so
the history page and restore keep working from CRs. A one-shot
migration in the agent at startup (like `migrate_ws_to_vol`): for each Workspace on this node
with a `volumeRef`, patch the Volume's ownerReference to the Workspace and write
`status.volumeRef`, `status.nodeName = spec.nodeName`, `status.compatibleNodes = [spec.nodeName]`. `spec.volumeRef` is dropped from the CRD schema afterwards
(pruned on read). The stuck `ws-16980a570dd6eecd` is deleted before the roll.

## Testing

- `crates/workspaces/tests/crd_yaml.rs`: schema still matches `deploy/k3s/crds.yaml`.
- `bins/agent/tests/reconcile.rs` (stub API server): a `SnapshotRequest` runs the push once and
  writes `done`; a second reconcile of a `done` request does nothing; a `working` request
  with no handle (restart) goes to `error`, never re-runs; a claim that hits 409 re-reads and
  does not overwrite; deleting a `working` request waits for the handle; an Environment stop
  creates one SnapshotRequest and deletes nothing until it is `done`; claim writes `nodeName` once; a Workspace
  with an unready Volume creates no pod; a `gitRepo` pod carries the init container with the
  key mount and no token; deleting a Workspace with an in-flight push still waits.
- `crates/workspaces/tests/api_user.rs`: create writes one object with empty `nodeName`; clone
  no longer copies a node; delete is one call.
- `tests/ws_e2e.sh`: add a phase — create a workspace seeded from a repo pushed earlier in the
  run, wait `Ready`, `kubectl exec` `git log -1` inside the pod.
