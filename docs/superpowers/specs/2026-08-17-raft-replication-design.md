# Replicated repo storage on local disk, with a routing proxy

Date: 2026-08-17 (supersedes `2026-08-16-sharding-design.md`)

## Problem

Repo metadata — refs, the repo row, the pack index — currently lives in SlateDB on S3. That makes
every ref read and write an object-store round trip. A push waits for the next WAL flush before it
is durable, which is ~50ms on the current 100ms `flush_interval`, and cold reads pay S3 latency
because nothing caches SSTs locally.

Objects are not the problem. Packs are content-addressed, immutable, and already served straight
from S3 by any node. Only the small mutable metadata is slow, and it is small: a few KB per repo.

The goal is local-disk latency for that metadata without giving up durability or the ability to
survive losing a node.

## Approach: RocksDB per repo, replicated by Raft, fronted by a routing proxy

Each repo's metadata lives in its own RocksDB instance on local disk, replicated to three nodes by
its own Raft group. One node is the leader and serves all traffic for that repo. The other two exist
for durability and failover only.

A proxy in front of the fleet routes each request to the repo's current leader.

Packs are unchanged: still in S3, still read directly by whichever node serves the request.

### Why local disk is now acceptable

The previous design relied on shared storage so that any node could open any repo, which is what
made failover free. Local disk gives that up. Raft buys it back: a repo is on three nodes, so losing
one loses nothing, and the survivors elect a new leader without moving data.

The trade is that nodes become stateful. Adding or removing a node means moving repo data, not just
recomputing a hash. That is addressed under "Membership changes" below.

## Components

### 1. Replicated store (`src/replica.rs`)

Replaces SlateDB as the metadata store. One Raft group per repo; the state machine is that repo's
RocksDB instance.

- **Log entries** are ref updates, repo creation/deletion, and pack-index updates — the same
  operations `refs.rs` and `store.rs` perform today.
- **Commit** happens once a majority (2 of 3) has the entry in its log. The push is acknowledged
  only after commit, so an acknowledged push survives the loss of any single node.
- **Reads** are served by the leader from its own applied state. No follower reads, so no staleness
  window and no read-index round trip.
- **Library**: `openraft`. It is async, Rust-native, and lets each group be a separate instance
  while sharing a network layer — which the heartbeat batching below requires.

### 2. Heartbeat batching

One Raft group per repo means one election timer and one heartbeat stream per repo. At a 100ms
heartbeat that is 20 messages/sec per repo per leader; at 10,000 repos a node would send tens of
thousands of RPCs/sec doing nothing.

So the Raft transport is shared across groups: messages between the same pair of nodes are coalesced
into one RPC carrying entries for many groups, and an idle group contributes a few bytes rather than
a message. This is the TiKV approach, and it is the reason per-repo groups are viable at all.

This is not an optimisation to add later. Without it the fleet does not scale past a few thousand
repos, so it is part of the transport from the start.

### 3. Placement

Which three nodes hold a repo is computed, not stored:

    replicas(repo) = top 3 nodes by rendezvous hash of (repo, node) over the live set

Rendezvous hashing is stateless — every proxy instance computes the same answer independently, with
nothing to persist and nothing to disagree about. Adding or removing a node moves only that node's
share of repos rather than reshuffling everything.

Placement picks the three *candidates*. Which of them is leader is Raft's decision, and changes on
failover.

### 4. Membership (`cluster/nodes/{url}`)

Nodes heartbeat to the object store every few seconds, writing a key with an expiry. The proxy lists
the prefix and treats non-expired entries as the live set.

The object store is used because every node and proxy already depends on it, so it adds no new
infrastructure, and because all proxy instances read one shared source and therefore converge. A
proxy that has a stale view routes to a node that will redirect it, so a brief disagreement costs a
hop, not correctness.

### 5. Routing proxy (`src/proxy.rs`, new binary)

Full proxy for both HTTP and SSH: all client traffic flows through it, node addresses stay private,
and clients see one address.

Routing rule, in full:

1. Parse the repo from the request.
2. `replicas(repo)` → the three candidate nodes.
3. Look up `repo → leader` in the proxy's cache; on a miss, pick any candidate.
4. Forward the request.
5. If the node answers "not leader, it is X", update the cache and forward to X.
6. If the node is unreachable, try the other candidates.
7. If no leader exists yet (election in progress), retry for a bounded window before failing.

The cache is a hint, never authority. It is repaired by the answer from the nodes themselves, so it
cannot drift into a wrong state that persists — a leader change costs one extra hop on the first
request after the election, and nothing after that.

**SSH requires termination, not TCP forwarding.** The repo path only becomes visible after the SSH
handshake, authentication, and the `git-upload-pack '/owner/repo'` exec request. So the proxy speaks
SSH: it holds the host key, authenticates the client against `auth/sshkey/` in the object store, and
then opens its own connection to the chosen node. This is the most involved part of the proxy and
the part most likely to be underestimated.

### 6. Node changes

`Role::Writer`/`Role::Reader` and `KLOUDLITE_ROLE` go away. A node's role for a repo is its Raft
role, which is dynamic. A node opens a repo's RocksDB when it is a replica for it, and answers
requests only when it is leader — otherwise it replies with the current leader.

The warm pool (`src/pool.rs`) survives with its rationale changed. Keeping 10,000 RocksDB instances
open is still undesirable, so on-demand open, single-flight, refcounted close, and the LRU/TTL bound
all still apply. What goes is the fencing logic: RocksDB uses a LOCK file rather than epoch fencing,
and the "reopen a fenced handle" path becomes "this node is no longer a replica for this repo."

## Data flow

**Push.** Client → proxy → leader. Leader validates, appends ref updates to the Raft log, replicates,
commits on majority ack, applies to RocksDB, answers the client. Packs went to S3 before the ref
update, as they do today.

**Clone/fetch.** Client → proxy → leader. Leader reads refs from local RocksDB and streams pack data
from S3.

**Node failure.** The dead node stops heartbeating; its groups elect new leaders among the remaining
two replicas. The proxy discovers this on the next request via a redirect. Repos where the dead node
was a follower are unaffected except that they are down to two replicas.

## Membership changes and rebalancing

This is the operationally hard part and it is deliberately minimal in v1.

When a node joins or leaves, `replicas(repo)` changes for its share of repos. Unlike the stateless
design, the new replica does not have the data and must receive a Raft snapshot from the leader.

v1 behaviour: rebalancing is a deliberate, operator-triggered pass rather than an automatic reaction
to membership change. A node that vanishes is tolerated (2 of 3 still forms a majority); restoring
the third replica is an explicit operation. This avoids a node blip triggering fleet-wide data
movement.

Automatic rebalancing, backpressure on snapshot transfer, and draining a node are out of scope here.

## Error handling

- **No leader (election in progress)**: nodes return a retryable status; the proxy retries for a
  bounded window, then fails the client with a clear message.
- **Not leader**: the node names the current leader; the proxy re-routes and updates its cache.
- **Minority partition**: a node that cannot reach a majority cannot commit, so pushes fail rather
  than diverge. Reads from a stale leader are prevented by leader leases.
- **Two of three lost**: the group has no majority and is unavailable for writes. This is
  intentional — the alternative is accepting writes that can be lost.
- **Proxy cannot reach any replica**: fail with the repo and the nodes tried, not a generic 500.

## Testing

- **Replicated store**: three in-process Raft nodes; assert a committed write is present on a
  majority, that a leader kill elects a new leader, and that an acknowledged push survives it.
- **Placement**: rendezvous hash is stable across processes, spreads repos evenly, and moves only
  the departing node's share when the live set changes.
- **Proxy routing**: leader cache is repaired by a "not leader" answer; a dead cached leader falls
  through to the other candidates; a no-leader window retries rather than erroring.
- **Heartbeat batching**: N idle groups between a node pair produce O(1) RPCs per interval, not
  O(N). This is the property the design rests on, so it gets an explicit test.
- **End-to-end**: existing `tests/protocol.rs`, `http_e2e.rs`, `ssh_e2e.rs` run against a
  three-node fleet behind the proxy, including a push that survives killing the leader.

## What this replaces

- SlateDB, `slatedb` dependency, and the S3-backed metadata path.
- `Role::Writer`/`Role::Reader`, `KLOUDLITE_ROLE`, and `DbReader` follower reads.
- The remaining shard/lease vocabulary in `README.md`, which still documents the deleted design.

## Migration

Existing deployments hold metadata in SlateDB at `slatedb` (single-shard) or `slatedb/shard-{i}`.
The pool reads `repo/{owner}/{name}`, and this design moves it off S3 entirely. A one-shot admin
command reads every repo's refs from the old store and replays them into the new Raft groups. Packs
in S3 are untouched.

## Known costs, stated plainly

- **Nodes become stateful.** Volumes, backups, and data movement on membership change — none of
  which the current design needs.
- **Three copies of metadata** instead of one shared copy.
- **Per-repo Raft groups are only viable with batched heartbeats.** If that transport does not hold
  up, the fallback is grouping repos into a fixed number of Raft groups, which is sharding again.
- **The proxy is a new always-on component** and a new failure domain, and it must be scaled and
  operated. The same routing is expressible in Envoy configuration; a bespoke proxy was chosen
  deliberately for control over leader-aware routing and SSH.
