# Ownership: pod zero holds the map, in memory, and nothing is replicated

A repo's database may be open on exactly one node. Two designs have now *derived* that fact — a
hash in a load balancer, then a rendezvous hash over a peer list — and each spent its complexity
reconciling the derivation with reality. This design stops deriving it. One node holds a map of
`repo → (node, expires)` in memory and is the only thing that decides who owns what.

That node is `rustic-git-0`. Not elected — named.

Nothing is written to disk, nothing is replicated, and no consensus protocol runs. If pod zero
restarts, the map goes with it, and every repo is re-claimed as requests arrive. That is not a
concession: ownership is soft state. The repos and their data live in blob storage and are never
touched by any of this. The map is a cache of *who is serving what right now*, and a cache is
allowed to be lost.

Supersedes the rank-and-probe rule in the peer-routing design. Keeps its forwarding (HTTP reverse
proxy, SSH byte pipe), its peer ports and secret, and fencing as the backstop.

## Why the previous two designs failed

Deriving ownership means the derivation's inputs must be perfect.

The first derivation was a hash in ingress-nginx. It could not route SSH at all, because the repo
name appears only inside an established session.

The second was a rendezvous hash over a peer list from DNS. A deploy showed DNS to be what it is —
a cache. It went stale during exactly the events that matter, dropped a restarting pod from the
set so its repos were re-ranked while it still held them, and returned an IP-derived name for a
pod that had a second Service pointing at it. Replacing DNS with the StatefulSet's static identity
fixed those, at the cost of no longer knowing which peers were *alive* — so liveness became a
probe, and probing grew a two-phase rule with vantages, timeouts, retries and caches to stop two
nodes acting on different views. Measured on a rolling restart, one failover cost about nine
seconds while clients gave up in one.

Every one of those is a node reaching its own conclusion about a global fact. One node holding the
fact removes the class.

## Leadership is a name, not a decision

`rustic-git-0` is the leader. Every node derives it from its own identity: strip the ordinal from
`RUSTIC_GIT_SELF`, append `-0`. There is no election, no lease on leadership, no heartbeat to
decide it, and no protocol to get wrong.

The property this buys is worth stating plainly: **two leaders cannot exist.** A StatefulSet
guarantees at most one pod per ordinal at any moment — it is the guarantee that distinguishes it
from a Deployment, and it is why the workload is a StatefulSet already. Since leadership is
identity rather than agreement, the split-brain that every election scheme must defend against
simply has no way to occur.

What it costs is availability of *claims* while pod zero is restarting. That is bounded, small,
and detailed under Failure modes.

**Do not add failover to ordinal one.** It would reintroduce exactly the problem this design
removes: each node deciding for itself whether pod zero is alive, two nodes disagreeing, two maps,
two owners. The whole value here is that leadership cannot be disputed. A leader that is
unreachable blocks new claims; it does not get replaced.

## Shape

```
        ┌──────── rustic-git-0 (leader) ───────────┐
        │  map: repo → (node, expires)   in memory │
        │  the only writer of ownership            │
        └────────────┬──────────────┬──────────────┘
             push    │              │  push
        ┌────────────▼───┐   ┌──────▼─────────┐
        │  rustic-git-1  │   │  rustic-git-2  │
        │  local copy    │   │  local copy    │
        └────────────────┘   └────────────────┘
```

* **Reads are local.** Every node keeps a copy and answers "who owns this repo?" from memory — a
  hashmap lookup, no network, nothing added to the request path.
* **Claims go to the leader**, over the peer port that already exists. One round trip, ~1ms
  in-cluster, and only when a repo is cold — not per request.
* **The leader pushes changes** as they happen, so a follower's copy is milliseconds behind rather
  than a poll interval. The map is at most a few dozen entries, so pushing it whole is simpler than
  computing deltas and costs nothing.
* **Pod zero is also an ordinary node.** It serves repos like any other; holding the map is an
  additional role, not a dedicated one.

## The map

Held only in pod zero's memory. One entry per **currently open** repo:

```
"alice/web" → { node: "rustic-git-1", expires: 2026-08-18T09:14:03Z }
```

Bounded by `nodes × RUSTIC_GIT_MAX_WARM` — at three pods and sixteen warm databases each, at most
48 entries, whatever the repo count. It does not grow with the number of repositories, because a
repo has an entry only while some node holds it open.

## Claiming, renewing, releasing

**Claim.** A node whose local copy shows a repo unowned or expired asks the leader. The leader
either grants it — recording the node and an expiry — or replies with the current owner. Because
one node decides, there is no race to resolve: a second asker is told who won and forwards there.

**Renew.** While a node holds a repo's database open it renews, batched — one message per node per
interval, covering everything it holds. The leader extends those entries and drops any expired.

**Release.** When the pool evicts a database — idle past `WARM_TTL`, or pushed out by `MAX_WARM` —
the lease goes with it, in this order:

```
1. tell the leader: expires = now + drain     ← still the owner, still serving
2. keep serving for `drain`                   ← followers whose copy is behind still arrive
3. close the database                         ← nothing points here now
4. the entry lapses; the next renewal prunes it
```

**Do not delete the entry outright.** Deleting makes the repo claimable immediately, so another
node can open the database while this one is still draining — and that open fences it. This system
has already produced exactly that failure on a real cluster: a terminating pod left the Service's
endpoints before releasing its repos, peers concluded they were ownerless, opened them, and fenced
a pod that was still serving.

**The database must be closed before the entry becomes claimable.** Invert steps 2 and 3 and the
fence is back.

## The lifecycle invariant

> **A node holds a repo's lease exactly as long as it holds that repo's database open.**

Neither half may outlive the other. A lease without a handle routes traffic to a node that must
cold-open before it can answer; a handle without a lease is the one writable handle to something
the map has given away. The pool drives both: a claim precedes an open, renewal continues while
the handle is held, and eviction begins with a release. A node that learns it has lost a lease it
thought it held closes that database at once rather than waiting to be fenced.

This is also what stops ownership calcifying. Without it a node would own a repo forever after
serving it once, and load would follow whoever received the first request after a restart rather
than current traffic.

## Failure modes

* **Pod zero restarts.** No new claims until it returns — about twenty seconds, measured on this
  cluster. Repos that are already open keep serving throughout: their holders have the databases
  and their renewals are advisory. When pod zero returns with an empty map, holders re-claim on
  their next renewal and the leader grants them, since nothing contradicts it. Cold repos claimed
  during the gap get a 503 and the client retries.
* **Pod zero is unreachable from one node** (partition). That node cannot claim; it keeps serving
  what it holds. It does **not** become leader. Other nodes are unaffected.
* **A follower dies.** Its entries expire and its repos are claimed by whoever next serves them.
  Detection is the lease TTL.
* **Everything restarts.** The map is gone and rebuilds from traffic. Nothing is lost, because
  nothing durable was ever there.
* **A stale grant is acted on.** SlateDB's writer epoch fences the second opener, the loser's pool
  reports it and re-routes. Noisy, bounded, never divergent data.

## Why this is safe without consensus

SlateDB's writer epoch is a genuine fencing token: a stale writer's writes are rejected by the
storage layer regardless of what any map says. Correctness has never depended on the ownership
mechanism and does not now. The leader buys *accuracy* — fewer wrong routes, less thrash — and is
worth exactly as much machinery as that is worth. Which is why the map is not replicated, not
persisted, and not run through a consensus protocol: every one of those would be protecting state
that is already disposable, underneath a guarantee that already holds.

## Latency

| Path | Cost |
|---|---|
| Owner serves | hashmap lookup, ~100ns |
| Forward to owner | hashmap lookup + one hop |
| Claim a cold repo | one round trip to pod zero, ~1ms, then the database open (50ms+) |
| Renew | background, off the request path |

Nothing is added to the steady-state request path, and the probe the current design performs on
the forward path is removed.

## What goes

`Membership`, `rank`, the two-phase `decide`, `/probe`, `probe_via`, probe timeouts, retries,
positive caching, single-flight, and hop-based failover. Roughly two thirds of `peers.rs` and a
third of `proxy.rs`. `RUSTIC_GIT_REPLICAS` stays — the leader's name is derived from the pod's own
identity, but the peer list is still needed for pushes.

## What stays

Forwarding, unchanged: HTTP as a reverse proxy, SSH as a byte pipe with a status-line handshake,
both over the secret-guarded peer ports. The hop bound stays as a loop guard. Fencing stays as the
backstop.
