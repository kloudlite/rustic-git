# Leader election for the git server tier — design

Date: 2026-08-29. Status: draft for review. Audit item I-2.

## Problem

`rustic-git-leader-0` is the leader by name. It alone writes the ownership map (one SlateDB
database at `cluster/ownership`, opened as the writer), every srv node forwards claims,
renewals and releases to it over `/own/*`, and `/healthz` on every srv pod goes un-ready when it
has not answered within `LEADER_SILENCE` (60 s). It runs in its own StatefulSet with
`maxUnavailable: 0`, serves no client traffic, and `deploy/roll.sh` exists to roll it before the
servers. If its node dies, no claim in the fleet succeeds until Kubernetes reschedules it and
its WAL replays — several minutes of "no node may safely serve this repository".

## What stays true

- One writer for the ownership map, always. SlateDB already enforces this at the storage
  layer: a second `Db::builder` writer on the same path fences the first (the
  `detected newer DB client` error every srv pod already knows how to react to via
  `on_fenced`). Election decides who *should* open the writer; SlateDB guarantees at most one
  actually writes.
- `App::route`'s invariant: a node never serves a repository on a failed claim unless it
  already held it and the repo is warm. Untouched.
- Followers read the map through `DbReader::open(FollowLatest)`; a change of writer is just
  a newer manifest to follow. Untouched.
- The `checkpoint` beat that keeps the leader's WAL bounded (two real crash-loops came from
  its absence). It runs on whoever holds the writer.

## Design

### The lease

One object in the object store, next to the map it guards: `cluster/leader`, body
`{node}\n{epoch}\n{expires_ms}`. It is written only with conditional puts (`object_store`
`PutMode::Create` when absent, `PutMode::Update(version)` otherwise — the first use of
conditional writes in the tree, wrapped in one helper, `ownership::lease`). Constants:
`LEADER_TTL = 10 s` (same as the repo lease TTL), `LEADER_RENEW = 3 s`.

Every srv pod runs the election loop:

1. Read `cluster/leader`. If it names me and is unexpired: renew (`Update(version)` with a
   fresh `expires_ms`, same epoch). If the update fails with a precondition error, I have
   lost the lease — demote (below).
2. If it is absent or expired: try to take it (`Create`, or `Update(version)` over the
   expired body) with `epoch + 1`. Exactly one candidate's put succeeds; the rest read the
   winner on their next tick.
3. Otherwise: remember the holder as the leader name and sleep `LEADER_RENEW`.

Ties are broken by the store, never by ordinal; there is no preference for a particular pod.
`RUSTIC_GIT_LEADER` and `leader_of(self_name)` are deleted; `App::leader()` becomes the name
read from the lease, updated by the loop (`set_leader` already exists).

### Promotion and demotion

Winning the lease promotes: the pod opens the ownership database as the writer
(`OwnershipStore::promote`, which is today's `open(.., leader = true)` moved behind a method),
starts the prune beat and the checkpoint beat, and answers `/own/*`. The open fences any
previous writer, so a stale leader that has not yet noticed losing the lease cannot write.

Losing the lease demotes: stop the beats, close the writer, reopen as a reader. Two things
force demotion — a failed renewal (precondition) and a fence error from SlateDB on any map
write. Both are already error paths; they now call one function.

The epoch rides on every map write as a fencing token: `grant_*` refuse when the App's
current epoch is not the lease's epoch (a stale leader mid-demotion), and the map entries do
not change format — the check is in-process, the storage-level fence is the backstop.

### Followers

`ask_leader_with` talks to `App::leader()` exactly as today. Two changes: a `421` (the pod
we asked is not the leader) or a connect failure re-reads `cluster/leader` before the next
attempt, so a failover completes inside the existing `Patience::Claim` budget (20 × 1.5 s)
without waiting for the loop's tick; and the leader itself keeps the in-process `grant_*`
path, so the pod that holds the lease never forwards to itself.

`/healthz` gates readiness on "a live leader exists": `is_leader()` or a lease read within
`LEADER_TTL` that is unexpired. A pod with no store access stays un-ready as today.

### What the leader does with repositories

Today's leader holds no repositories (`grant_claim` hands its own claims to `least_loaded`).
With any pod able to lead, that carve-out goes: the leader serves like every other node.
`servers()`, `leader_of`, `with_topology` and the srv/leader prefix split are deleted.

### Deploy

`rustic-git-leader.yaml` and its PDB are deleted; `rustic-git-srv` keeps
`minAvailable`-style PDB (already there) and its ordinals are all candidates. `roll.sh`
becomes one `kubectl apply` plus `rollout status` — the two-phase order existed only for the
name-based leader. The headless Service keeps `publishNotReadyAddresses: true` (peer DNS
before readiness) and stops selecting a leader role. `RUSTIC_GIT_LEADER` disappears from both
manifests and `pin.sh`'s comment.

Rollout: the new build must start with the old leader still running. The old leader holds
the SlateDB writer but knows nothing about the lease; the first new pod takes the lease and
opens the writer, fencing the old leader, whose `/own/*` handlers then return errors until it
is rolled away. Order for the one migration: roll `rustic-git-srv` to the new build first,
then delete the leader StatefulSet. Written into `deploy/RECOVERY.md` and the commit.

### Failure modes

| Failure | Behaviour |
|---|---|
| Leader pod dies | Lease expires ≤ 10 s; next tick a peer takes it (≤ 3 s), opens the writer (WAL replay, bounded by the checkpoint beat); claims in flight retry within `Patience::Claim`. Worst case ~15 s plus replay. |
| Leader loses store access but keeps running | Its renewals fail → demotes; a peer takes the lease. Its map copy stops updating and `/healthz` goes un-ready once the lease read fails. |
| Two pods believe they lead (renew raced an expiry) | Only one holds the newest epoch; the other's writer open is fenced by SlateDB, or its next write fences — either way it demotes. Never two writers. |
| Object store conditional put unsupported (`file://` dev) | Solo mode (single node) skips election entirely as today; a multi-node `file://` fleet is only the test harness, which uses `mem://` (conditional puts supported). Documented; `file://` election is refused at boot with a clear error rather than silently unfenced. |
| Clock skew between pods | `expires_ms` is written by the holder from its own clock; a skewed reader may take the lease early (fenced by SlateDB) or late (a longer outage). Bounded by the TTL; same exposure the repo leases have today. |

### Not in scope

Multi-region leaders (one map per region already); changing the map's storage; per-peer
force-claim budgets (`FORCE_MIN_AGE` stays as is).

## Tests

- `ownership::lease` unit tests on `mem://`: create wins once; expired lease can be taken with
  epoch+1; renew with a stale version fails; concurrent takers — exactly one succeeds.
- `crates/app`: promote/demote state machine with a fake lease; a fenced write demotes; grants
  refuse a stale epoch.
- `tests/routing.rs`: a three-node fleet elects one leader; kill it (drop its App) → another
  takes the lease and claims succeed within the patience budget; the old leader's late write
  is refused; `/healthz` reflects "no live leader" during the gap.
- Deploy: manifests parse; `roll.sh` `bash -n`; e2e (`registry_e2e.sh`/`http_e2e`) unchanged.
