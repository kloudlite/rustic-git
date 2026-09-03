# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```sh
cargo test                                   # workspace: every crate's unit tests plus tests/*.rs
                                             # integration suite (the root package, `rustic-git-tests`,
                                             # is a near-empty lib that only hosts tests/)
cargo test --test registry_blobs             # one integration test file — still runs from the root
cargo test --test registry_http some_name    # one test by name
cargo clippy --workspace -- -D warnings      # CI gates on this (image.yml test job): every lib and
                                             # bin, test targets excluded; --all-targets still has
                                             # pre-existing lints in test targets — the bar there is
                                             # no NEW warnings in files you touch.
./tests/registry_e2e.sh                      # real docker push/pull round trip; exit 77 = the
                                             # docker half was skipped (no daemon) — not a pass
./tests/ws_e2e.sh                            # real server+api+agent+Azure+btrfs workspaces
                                             # round trip against a k3s cluster (still three
                                             # binaries — the agent is a controller now, not a
                                             # poller — and rustic-git-api serves /v1/*);
                                             # exit 77 = a prerequisite (root-capable btrfs, a
                                             # reachable cluster with the CRDs installed,
                                             # AZURE_* env) was missing — needs a Linux VM
                                             # with btrfs and k3s, not this Mac

cd web && bun install
bun run dev / lint / typecheck / build / test # turborepo; app in web/apps/web; test = bun test
                                             # (*.test.ts are excluded from tsc — no bun-types)
```

Run a server locally without S3: `RUSTIC_GIT_S3_URL=file://./x` (or `mem://`, lost on exit).
Local scratch (host key, cache) defaults under `./.local/`, which is git-ignored.

Workspace layout: `crates/{core,storage,gitbase,pulls,app,git,registry,api,workspaces}` are the
library crates; `bins/{server,api,worker,agent,gateway,kl}` build the six binaries (`rustic-git`,
`rustic-git-api`, `rustic-git-worker`, `rustic-git-agent` — the agent is root-only and runs as a
DaemonSet, one per btrfs-capable node, see "Workspaces and environments" — `rustic-git-gateway`,
the workspace SSH tunnel, and `kl`, the user CLI, which is built by `kl.yml` and never deployed);
the root package is `tests/`'s host only, not a facade.

## The one invariant everything hangs off

One SlateDB database per repo, and **exactly one node may have it open**. The routing middleware
in `bins/server/src/router/route.rs` (`repo_of` → `route_inner`) derives an ownership key from the URL **before
authentication** and refuses anything it cannot route, because opening a database on the wrong
node fences the legitimate owner (a `Closed error: detected newer DB client` in logs means this
happened). The ownership map has one writer, **elected**: every `rustic-git-srv` pod runs `App::election_tick`
every 3 s, and the pod holding the lease at `cluster/leader` (`crates/storage/src/ownership/lease.rs`
— conditional puts only, TTL 10 s) opens the map as WRITER (`OwnershipStore::promote`). The lease
epoch is checked under `leader_lock` on every map write, a fenced write demotes, and SlateDB's own
writer fence is the backstop; followers re-read the lease when the node they asked answers 421 or is
unreachable. There is no leader pod, no `RUSTIC_GIT_LEADER`, and no preferred ordinal; a dead leader
is replaced in ~15 s. A multi-node `file://` store is refused at boot (`LocalFileSystem` has no
conditional update). When adding any
route that touches a per-repo/per-image database, it must route —
`BROWSE_TAILS` in `bins/server/src/router/route.rs` is the contract, and `every_browse_route_is_routable` holds the
router and the middleware together. A handler that only reads the shared object store may be
served on any node (that is why `/api/{owner}/images` and `_catalog` are exceptions).

## Two namespaces, one server

- Git repos: DB at `repo/{owner}/{name}`, routing key `{owner}/{name}`.
- Container images (OCI registry, `/v2/...`): DB at `repo/img/{owner}/{name}`, routing key
  `img/{owner}/{name}` (`crates/registry/src/`). `api`, `v2`, `img` are reserved owner names so the two
  keyspaces cannot collide. An image is NOT tied to a repo of the same name.

Registry layout in the object store: blobs `blobs/{owner}/{algo}/{hex}` (per-owner, shared
across that owner's images), manifest bytes `manifests/{owner}/{name}/{algo}/{hex}`. Tags,
upload sessions, referrer rows, pull counters live in the image's own DB (single writer ⇒
atomic tag updates).

## Load-bearing rules (violations have all been real bugs)

- **Only two things ever delete a blob**: an explicit client `DELETE /v2/.../blobs/{digest}`,
  and the GC sweep (`crates/registry/src/gc.rs`) — never a manifest path, because siblings share
  layers. The sweep is keep-biased: any uncertainty (unreadable manifest) aborts it.
- **Manifest bytes are stored and returned verbatim.** The digest is over those exact bytes;
  parse to read a field, never re-emit.
- **`Digest::parse` is the only way a path segment becomes an object-store key** (sha256 or
  sha512, lowercase hex, exact length). Upload-session uuids are validated the same way.
- Every `/v2` error is the OCI envelope via `registry::oci_err`; auth flows through
  `registry::auth::allow` (Basic and Bearer both; anonymous ≠ invalid credential — an
  anonymous token from `/v2/token` must keep working for public pulls).
- Registry blob routes have their own body limit (`max_layer`, default 5 GiB, matching S3's
  single-request CopyObject cap) separate from
  the git `max_body` (2 GiB); manifests have a third. Check which limit applies before assuming
  a 413 is the handler's.
- The browse API mounts on the **peer listener only**; the public listener 404s `/api/`.
  Credentials live as plain object-store keys (any node authenticates), not in SlateDB.
- **Markers under `index/` are views for listings, never authorization.** Owning nodes write them
  and reconcile their visibility; the GC worker reconciles their structure.
- **The same rule governs the CRDs' `rustic-git.io/owner` and `/kind` labels.** `spec.owner` is the
  truth; the labels are a view of it, and exist only because label selectors are indexed by the API
  server while an arbitrary spec field is not (adding a `selectableFields` entry per query axis is
  how a CRD becomes a database). `/v1` stamps them at create and the node controller RE-STAMPS them
  on every reconcile (`heal_labels`), so an object written by any other path — a restored backup, a
  migration, an operator with kubectl — becomes listable rather than being owned correctly and
  invisible forever. Never authorize on a label; `may_act_on` reads `spec.owner`.
- **The `events` Redis stream (`crates/storage/src/events.rs`) is a nudge for the worker and a view for the
  activity feed, never the record.** Every consumer that matters keeps a fallback that doesn't
  depend on it (the owner's periodic check/announce beats in `bins/server/src/lanes.rs`) — verified
  to still work with Redis entirely down. The one exception is deliberate: the PR half of the
  activity feed is stream-only (`feed.rs`, "no fallback here on purpose"), so with Redis down the
  feed goes quiet on PR events and keeps only `repo_created`.

## PR merges live in the worker, not the server

The owning node only RECORDS merge state (claim/outcome/mergeability — three peer-only routed
endpoints in `bins/server/src/browse_api/pulls.rs`) and re-announces stranded jobs on a 15s beat
(`announce_stranded_merges` in `bins/server/src/lanes.rs`). The actual merge runs in `rustic-git-worker` using the real
`git` binary (`crates/pulls/src/merge_worker.rs`): bare cache under the worker's cache dir, fetch/push over
the peer listener with `-c http.extraHeader` peer auth, `merge-tree --write-tree` for
merge/squash, a throwaway worktree for rebase, `push --force-with-lease` against the oid the
merge was computed from. Traps that were all real: the server speaks upload-pack protocol v2
ONLY, and libgit2 has no v2 — git2/libgit2 cannot fetch from this server; pods have no git
identity, so every commit-writing git call must set GIT_COMMITTER_*/GIT_AUTHOR_* env; a retried
squash is caught by merged-tree == base-tree, not by ancestry or the lease. The `local()` vs
`networked()` split in `merge_worker.rs` is what keeps the peer secret out of error messages —
never format a networked argv into anything.

Fetch packs are built with `TreeAdditionsComparedToAncestor` plus a full-tree second pass for
merge commits — gix-pack drops all-but-last-parent additions on a merge
(GitoxideLabs/gitoxide#2935); delete the workaround in `crates/git/src/protocol/upload/pack.rs` when that fixes.

## Workspaces and environments

`crates/workspaces` + `bins/agent` (`rustic-git-agent`) + `bins/api` (`rustic-git-api`) add a
second, unrelated control plane: btrfs-backed dev workspaces and multi-service environments,
separate from git storage, and separate from the registry too: a workspace/environment's pushed
state is a `Snapshot` CR plus a read-only btrfs subvolume on each node that holds it. Nothing about
it goes through the server tier, and nothing about it goes to an object store.

**Kubernetes is the reconcile substrate, and the CRDs are the source of truth.**
`crates/workspaces/src/crd.rs` defines `Volume`/`Workspace`/`Environment` in
`rustic-git.io/v1alpha1`, all cluster-scoped: `/v1` on `bins/api` writes **spec** (desired), each
node's controller writes **status** (observed) through the `/status` subresource, and RBAC plus
the ValidatingAdmissionPolicy in `deploy/k3s/agent-admission.yaml` — not convention — is what
stops a controller editing desired state: the agent's ClusterRole (`deploy/k3s/agent-rbac.yaml`,
whose header table IS the role) keeps `patch` on the main resources only for labels, finalizers
and the two spec fields a parent's reconciler copies into its own child (`Volume.spec.restoreTo`,
and `Volume.spec.quotaGb` on the home volume an `OwnerBinding` owns),
and the policy refuses it any other spec change. Apply both files.

**Allocation is bounded by a `Quota` per owner.** A cluster-scoped `Quota` CR named by the owner
slug (a person or a team) caps six dimensions — workspaces, environments, snapshots, diskGb, cpu,
memoryGb — with `default-user`/`default-team` as the fallback for an owner who has none and a
compiled-in table (`crd::default_quota`) behind those, so a missing object is never "unlimited".
**Usage is computed from the CRDs on every request and never cached** (`crates/workspaces/src/quota.rs`):
a stored counter can only be wrong in the direction that hands out allocation nobody has. `/v1`
refuses an over-quota create, restore, clone or push with `409` and one sentence — `"{dimension}:
{used} of {limit} in use; request more under Quota"` — from `quota::refuse`, through the single
gate `guard_alloc`; the check is read-then-write, so two concurrent creates can overshoot by one
and the agent's per-namespace `ResourceQuota` (named `owner-quota`, written on every `OwnerBinding` and environment
reconcile, from the same effective `Quota`) is the hard stop for cpu and memory. A raise is a
`QuotaRequest` CR: the owner, or a team member whose directory role is at least admin, opens ONE
pending request at a time; only a superadmin approves, which writes the `Quota` **before** marking
the request.

**Superadmin is a claim, not an owner.** `superadmin: true` in the session JWT, minted at sign-in
from a `superadmins` collection in the directory that `RUSTIC_GIT_WORKSPACES_ADMINS` merely
bootstraps at boot (additive only — dropping an address from the env revokes nobody; the list is
managed from then on through `/api/admin/superadmins`). The admin surfaces run as a SEPARATE
process, `bins/api` started with `RUSTIC_GIT_API_ROLE=admin` instead of the default `user`, which
mounts `api::admin::router()` in place of `api::router()` — the user router has no admin handler
at all, so there is no code path in the ordinary process that could leak one. `refuse_without_claim`
(`crates/workspaces/src/api/admin.rs`) 403s a token without the `superadmin` claim before any route
runs, and region creation, quota decisions and `/api/admin/superadmins` all live only behind it.
RBAC mirrors the split: `api-rbac.yaml` gives the `rustic-git-admin` ServiceAccount write on
`Quota`/`QuotaRequest`/`Region` and keeps the ordinary `rustic-git-api` ServiceAccount to reads,
so a bug that mounted the wrong router would still 403 at the Kubernetes layer. The web's `/admin`
area calls this second process at `RUSTIC_GIT_ADMIN_API_URL`, never the ordinary API base — mixing
them up would silently point an admin page at `/v1` instead.

There is no job queue, no lease,
no agent registration and no long poll: `/v1` writes ONE unplaced object and establishes no facts
about it — the node controllers CLAIM it (a guarded write of `status.nodeName`, admitted for the owner
node always and for any other node only while it is up to date for that worktree), so two nodes can never contend for the same subvolume and the API never
places anything. When a node is unplaceable — dead for `WS_NODE_DEAD_SECS`, or labelled
`rustic-git.io/decommission=true` (`peer::unplaceable`, one predicate for both) — the sweep decides
PER VOLUME, never per parent (`peer::volume_decision`): any Running parent on it pins the whole
volume (`Unavailable`, `Degraded=True/NodeDead` on every parent, nothing moves); otherwise any
stopped parent that is not yet `Replicated` pins it too; otherwise the pin is cleared and every
parent un-placed, so an up-to-date node claims them on the next start. A running worktree is
*interrupted*, not moved: `/v1` refuses to start it (409, "its node is down; it resumes when the
node returns") and the way forward is a clone from the last synced point, whose `based_on` states
that cut's age (an environment has no such fallback — its clone copies live bytes, so an
interrupted environment is a 409). A decommission is the planned version of the same thing with one
difference — whatever runs there keeps running: the node's own agent beats every
`WS_DECOMMISSION_SECS` (30), tells its running parents (`Decommissioning=True/NodeLeaving`),
releases each volume as it becomes releasable (reason `Decommissioned`, and never marks a running
one `Unavailable`), and stamps `rustic-git.io/decommission-status: draining running=N owned=N
copies=N thin=N` (`thin` = volumes whose bytes are still here and which other nodes hold fewer than
`spec.replicas - 1` Synced copies of) until it can stamp a sticky `drained <RFC 3339>`, the gate on
deleting the VM. Independently of any of that, every replica the dead
node held is healed onto a live third node automatically — placement stops naming dead nodes as
candidates and retires a copy once its replacement is Synced (`live_nodes`/`retire_pass`) — because
healing a COPY risks nothing, unlike moving the live worktree. A released volume's pin is cleared,
and the node that then claims the parent takes it with a JSON-patch `test` on the empty value
(`take_volume`), the one other spec write the admission policy allows. `crd::Volume` is separate from `Workspace`/`Environment` on purpose — both own
exactly one btrfs subvolume with identical semantics — and it is REFERENCE-COUNTED, in
ownerReferences: the parent's controller creates it as an owner of it, and a `Snapshot` a push
writes is owned by the Volume. A volume lives while a working copy or a snapshot references it
and is collected when neither does. Deleting a workspace or an environment therefore runs the
`WORKTREE_FINALIZER` (`cleanup_parent`): drop the worktree, delete that working copy's sync
points, and detach the Volume — remove the parent's owner entry — only if a snapshot remains, so
the volume survives detached with its snapshots; with none left the entry stays and Kubernetes GC
takes the Volume and its records — the bytes go on the agent's next beat, when the orphan-voldir
sweep finds a tree with no Volume behind it. A lost detach is an error, never a completed
finalizer. `retire_pass` in `bins/agent/src/peer/sweeps.rs` is the safety net at both ends: it deletes
`snap/` subvolumes whose record is gone (re-read before every delete, keep on any error) and
deletes a Volume that has no owner entry and no snapshot. Containers live in
a namespace per owner or environment (`crd::ws_namespace` → `ws-{owner}` / `wt-{owner}-…` for a
team, `env-{id}`): a workspace is one bare Pod, an environment's services are StatefulSets.
`desiredState: Stopped` deletes the workspace pod (and, for an environment, its StatefulSets once
the stop push has landed); the controller re-reads spec on every reconcile, which is how a stop
survives a node reboot. Service-to-service DNS comes from CoreDNS, so `mongodb://db:27017`
resolves inside an environment's namespace. `model::validate_mount` still runs on every mount and
is still load-bearing — a hostPath source escapes just as a bind source did, and the API server
will happily mount `/` if we ask it to.

A workspace may be attached to ONE environment (`Workspace.spec.attachedEnvironment`, written only
by `/v1`), and then resolves that environment's services by bare name. The mechanism is a
`/etc/resolv.conf` the agent renders per workspace into `{pool}/attach/{ws}/resolv.conf` and every
pod mounts read-only through a hostPath volume of `type: File` — the volume IS that file, so there
is no `subPath` — `dnsConfig` is immutable on a running pod, so the mount is what makes attach and detach
take effect without a restart. That file is written IN PLACE and never renamed: the pod holds the
inode, so a rename would leave it reading the old file forever. Two NetworkPolicies named
`attach-{ws}` open the path, selecting the workspace POD (siblings share a namespace); the
environment-side one is owned by the Environment because an ownerReference cannot cross namespaces.
There is no Workspace finalizer for this: `/v1`'s `delete_ws` removes the environment-side policy
itself while the spec is still readable, and the agent's janitor sweeps orphaned `{pool}/attach/{id}`
directories left behind by a workspace that is simply gone.

`Region` is a cluster-scoped CRD (`crd::Region`) like everything else here — `bins/api` is its only
writer, via `/v1/regions` (server-side apply, so a second POST of the same id retires or renames it
rather than 409ing). Snapshot BYTES have
no object store at all: a snapshot is a read-only btrfs subvolume under `{pool}/vol/{volume}/snap/`,
and it reaches other nodes as a `btrfs send` streamed over the peer listener between agents
(`bins/agent/src/peer/pull.rs`) — never uploaded anywhere. Durability is therefore replica count
(`Volume.spec.replicas`, placed by `replicate::targets`), not a blob container.

Four verbs — push, restore, clone, delete — and no separate commit step. `push` is the single
mutating verb: it takes a **snapshot** of the working copy, kept until somebody deletes it, and it
is the only thing that keeps a volume alive once its workspace or environment is gone. `/v1` writes
a `Snapshot` CR (`spec.transient: false`, owned by the Volume) naming the volume's current head as
its parent and the owning node fulfils it:
snapshot + upload + mark `Ready` + advance `status.head`, with an optional message
(`GET /v1/volumes/{name}/history|refs` on `bins/api` reads the chain of `Ready` `Snapshot`s back).
Every cut also records `spec.state` (`crd::SnapshotState`), the parent's own definition — image,
packages, resources, quota, attached environment for a workspace; services and quota for an
environment — frozen at that instant, and `restore` defaults to whatever it froze (the request
body's fields override it; an environment always has services, so an empty `services` list means
the snapshot's, and a snapshot that froze none needs them in the request). A
workspace created with `repo`/`branch` is seeded by an init container that clones it over SSH with
the owner's platform key, inside the workspace pod itself — no credential Secret is minted for it.
There is no user-facing un-pushed state; internally
`push` still stages a local RO snapshot before uploading it (the split survives only as a
crash-recovery seam — a push that dies mid-flight leaves the stage files and an internal
`unpushed` mark so a retried push picks them up, never re-snapshotting stray data or losing it).
`clone` (`POST /v1/workspaces/{id}/clone`, the one local-copy verb — "fork" appears nowhere
user-facing) cuts its OWN sync point at the moment of the request — a `clone-{ws}-{hex}` transient
parented on the source's newest one — rather than leaning on whatever the last beat left, and the
response's `based_on` (`snapshot`, `at`, `age_seconds`, `interrupted`) always names what it grafted
onto. It places by the one up-to-date rule like everything else, which is why a running source's
clone lands on the owner: at the instant of the cut nothing else holds it. An INTERRUPTED source
cannot be cut at all, so its clone grafts onto the newest transient an up-to-date node already
holds and `based_on` states that age — the one way forward, chosen knowingly. An ENVIRONMENT clone
is the exception in kind: it still copies bytes from the source's own live subvolume on the node
that holds it, so it cuts nothing, carries no `based_on`, and refuses an interrupted source with a
409 — there is nothing on a live node to copy from. `restore` (`POST /v1/workspaces/restore`) instead grafts onto
an explicit past **snapshot**, named by id, and RE-ATTACHES: the new working copy is a worktree of
that snapshot's volume and an owner of the Volume again, even when the volume was detached and the
source is long gone. `delete` is the only explicit verb on a snapshot —
`DELETE /v1/volumes/{name}/snapshots/{id}` refuses a sync point and a running worktree's base
(409 both), and deleting a detached volume's last snapshot deletes the volume;
`DELETE /v1/volumes/{name}` takes a detached volume with all its snapshots and refuses one that
still has a working copy. Everything else the agent cuts is a **sync point** — internally
`spec.transient: true`, owned by the working copy rather than the Volume, never listed as history,
never a restore target, and gone with the working copy. Between pushes, a background
sync beat (`WS_SYNC_SECS`, `bins/agent/src/sync.rs`) cuts one — never a parent,
never advancing `status.head` — from each running worktree whose btrfs generation has moved or
whose definition (`spec.state`) has changed since its newest sync point, so a
peer node's replica always has something recent to fetch; retain prunes sync points only (exactly
one Ready per worktree — a push is never pruned), and a node re-hosting a worktree checks out the
newest one it holds locally before falling back to `status.head`. The migration baseline is a sync
point too, and one written by an older build is recognised by its shape
(`crd::Snapshot::is_snapshot`), so nothing had to be migrated. The agent
(`rustic-git-agent`, privileged, one pod per btrfs-capable node) is a controller, not a worker:
it watches its own node's objects and converges them (`bins/agent/src/controller/`), and its
identity is `$NODE_NAME` from the downward API, its liveness the DaemonSet's own probe. It talks
to the k3s API and to OTHER AGENTS' peer listeners (`WS_PEER_SECRET`, btrfs send over HTTP), and to
nothing else — no object store, no Azure credential, and no HTTP service of ours.
Stopping a workspace or environment cuts a `stop-{ws}-{gen}`/`stop-{env}-{gen}` sync point, named by
the parent's generation so every stop is a fresh cut (skipped if the pod never ran), and tears
the pod (or the StatefulSets) down as soon as that cut is Ready — a stop is seconds, and it never
waits for a replica. Right after the cut the owner POSTs `/peer/v1/wake` to every placeable node
(`peer::wake_peers`) so the peers pull within seconds instead of at the next replication beat. Stop,
clone and push cuts wake unconditionally; a sync cut wakes too but COALESCED to at most one wake per
node per `WS_SYNC_SECS` (`snapshot::wake_worthy`, the timestamp on `Ctx::last_sync_wake`), so an edit
reaches a replica in about one sync beat rather than waiting out the five-minute pull. Whether the bytes have landed elsewhere
is the `Replicated` condition on the parent (`controller/stop.rs`), computed only by the owner:
`False/Running` in the same status write that records a running pod, and while stopped
`False/AwaitingReplica` ("no other node holds the final sync point yet", or "no replica is
configured for this volume" when `spec.replicas: 1` means it never will) until another node's
`VolumeReplica` holds the cut BY NAME, then `True/Replicated` ("another node holds the final sync
point"). It is read everywhere else — `/v1`, the web, the dead-node sweep, placement. **The wait
moved into placement**: a stopped parent may start on ANOTHER node only once that node is up to
date for the worktree — its `VolumeReplica.status.branches[worktree]` names that worktree's newest
Ready transient — and until then the only place it can start is its own node. `may_claim`
(`bins/agent/src/claim.rs`) is that one rule; `compatibleNodes` is a dead field, kept only so old
stored objects parse. Starts also SPREAD: when a volume is movable (nothing on it running) its
owner computes the preferred node by rendezvous (`peer::preferred_node`) over `{owner} ∪ {up-to-date
nodes}`, keyed by the volume id, and hands the volume over (release CAS + un-place every parent,
`Placed=False/Moving` — a routine move, never `Degraded`) when that is not itself.

**Every person has one persistent home per region, not per node** — `/home/kl` in every workspace
pod of theirs is `{pool}/homes/{owner}` on a region-shared NFS export served by ZeroFS, mounted by
every node at `{pool}/homes` (`mount_homes` in `bins/agent/src/lib.rs`, `WS_HOMES_EXPORT`). There
is no home `Volume` CR, no owner→node pin, no push beat, no history and no quota — all deliberately
dropped: an NFS directory has no qgroup to enforce one and no per-commit history to keep. Making
the export directory exist (`ensure_shared_home` in `bins/agent/src/controller/workspace.rs`, plain
`mkdir`+`chown`, safe on every reconcile) is the whole "materialize a home" story now. A pod
started before its node's NFS mount is up would hostPath an empty local directory and silently
strand the owner's dotfiles, so `apply_workspace` parks a workspace in `Creating`/`HomeNotReady`
until `ctx.homes_export` is set rather than ever starting one.
Tool caches and the editors' remote servers must not live on the shared
export — concurrent pods on different nodes would race the same cache directory and every cache
hit would cross the network — so they are redirected (`login_env`'s `XDG_CACHE_HOME`,
`CARGO_TARGET_DIR`, etc.) into a per-(owner, node) LOCAL cache subvolume, `{pool}/homecache/{owner}`
(`Engine::ensure_homecache`), mounted at `k8s::HOME_CACHE_DIR`. Shell history and
`~/.local/state` (`k8s::HOME_STATE_DIR`) are local for the same reason — one file, many terminals,
many nodes — and share that same `homecache` volume via a separate subPath. Cross-region: each
region has its own export and nothing syncs them.

A profile is keyed by `packages::hash(pin, base + spec.packages)` and indexed per node at
`{PROFILES_DIR}/by-inputs/{hash}` → the store path, so a second workspace or a clone with the same
inputs is published straight from the index and never invokes nix (an evaluation of nixpkgs costs
~28 s cold, ~0.3 s warm). A dangling entry is a miss, never a profile with an empty `bin`; the
janitor sweeps entries no `{id}/current` resolves to. The derivation name carries no workspace id —
it used to, which is what stopped two identical package sets sharing one store path.

## Web app

Next.js app router in `web/apps/web` (its own `CLAUDE.md`/`AGENTS.md` there warns the installed
Next.js differs from training data — read `node_modules/next/dist/docs/` when unsure). One shell
(`components/app/app-shell.tsx`) renders all chrome; `shell-nav.tsx`'s `place()` classifies the
URL as org / repo / image and picks the tab row — reserved names in `store::RESERVED_REPO_NAMES`
are what make that unambiguous. Copy existing siblings, not new patterns: `repo-list.tsx` for
filterable lists, repo `settings/` for destructive actions, `lib/time.ts` for size/date
formatting. Tokens over raw Tailwind colors; `--radius: 0` — sharp corners everywhere.
Editor TS diagnostics here are frequently stale; trust `bunx tsc --noEmit -p apps/web/tsconfig.json`.

## Deploying

CI builds images tagged with the commit SHA on push to master — **only if that commit's test job
passed** (`image.yml`'s image job `needs: [build, test]`), so a red commit has no package at all
and a repin to it is an ImagePullBackOff, not a bad deploy. `web.yml` only runs when `web/**`
changed, so the two images do NOT move in lockstep; pin each yaml to the last SHA that actually
built that image. Flow: push → wait for the run → `deploy/pin.sh <sha> [web-sha]` (rewrites every
pin in `deploy/` — server, api, worker, agent, gateway from one SHA, web from the other — and
refuses a SHA with no package) → commit → `deploy/roll.sh` (one apply, then the rollout waits; the k3s side is applied by
hand per `deploy/k3s/README.md`). The StatefulSet roll moves DB ownership between nodes, and the
map's writer moves with the lease when the holder rolls (≤ one TTL plus one tick); the first registry request to a moved image
can 500 once (known fenced-handle gap). The registry hostname (Cloudflare-proxied — verify with `dig` before touching ssl-redirect) and the app
hostname are different ingresses with different TLS assumptions — read the comments on both
Ingress objects before touching them. The worker liveness probe counts per-lane heartbeat files
and the web probes hit `/api/health`, so a yaml roll must never outrun its image repin. The
`rustic-git-jwt` Secret is required (pods fail closed without it), and Rust pods run as uid 1001
with a read-only root — anything new that writes to disk needs a mount.

## House style

Comments explain WHY, never what; match the density of `bins/server/src/router/route.rs`. Deliberate shortcuts are
marked `// ponytail: <ceiling and upgrade path>` — keep the marker when editing near one.
Commit subjects are imperative sentence case with no tool attribution. Design docs and plans
live in `docs/superpowers/`; the README's deep sections (ownership, write throughput, container
images) are accurate and worth reading before touching those areas.
