# Ownership: pod zero writes the map, the others read it

A repo's database may be open on exactly one node. Two designs have now *derived* that fact — a
hash in a load balancer, then a rendezvous hash over a peer list — and each spent its complexity
reconciling the derivation with reality. This design stops deriving it. One node writes a map of
`repo → (node, expires)` and is the only thing that decides who owns what.

That node is `kloudlite-0`. Not elected — named.

The map lives in its own SlateDB database, `cluster/ownership`, alongside everything else this
system stores. Pod zero opens it for writing and is the only writer; every other node opens it
read-only and follows. No consensus protocol runs, because SlateDB already permits exactly one
writer and fences any second one — the same mechanism that protects every repo protects the map.

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

`kloudlite-0` is the leader. Every node derives it from its own identity: strip the ordinal from
`KLOUDLITE_SELF`, append `-0`. There is no election, no lease on leadership, no heartbeat to
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
                    cluster/ownership  (SlateDB on blob)
                      repo → (node, expires)
                    ▲              │        │
             writes │         reads│        │reads
        ┌───────────┴──────┐  ┌────▼─────┐  ┌▼─────────┐
        │  kloudlite-0    │  │ -1       │  │ -2       │
        │  sole writer     │◄─┤ claims   │◄─┤ claims   │
        └──────────────────┘  └──────────┘  └──────────┘
                     claims travel to pod zero over the peer port;
                     everyone reads the map directly
```

* **Reads are local to the follower's own reader.** Every node answers "who owns this repo?" from
  its read-only handle on the ownership database, which SlateDB keeps current by polling the
  manifest. No network call on the request path.
* **Claims go to pod zero**, over the peer port that already exists — followers cannot write. One
  round trip, and only when a repo is cold, not per request.
* **Pod zero is also an ordinary node.** It serves repos like any other; writing the map is an
  additional role, not a dedicated one.

### Two tuned intervals

A follower's view is as old as its last manifest poll, and a claim is only granted once the write
is durable. Both are the defaults' fault, not the design's, and both are set explicitly:

| Setting | Default | Here | Why |
|---|---|---|---|
| `manifest_poll_interval` (followers) | 1000ms | **200ms** | bounds how stale a follower's routing view can be; costs a few manifest GETs per second |
| `flush_interval` (ownership DB) | 100ms | **10ms** | a claim waits for this flush; the map is tiny and write-light, so a short interval is cheap |

**Follower staleness is harmless, and that is the property the design rests on.** Only pod zero
grants, so a follower reading a stale map forwards to a node that no longer owns the repo; that
node consults pod zero and forwards again or claims. One wasted hop, self-correcting, bounded by
the hop count. It cannot produce two owners, because a follower's belief never grants anything.

## The map

Written by pod zero to `cluster/ownership`, one key per **currently open** repo:

```
"alice/web" → { node: "kloudlite-1", expires: 2026-08-18T09:14:03Z }
```

Bounded by `nodes × KLOUDLITE_MAX_WARM` — at three pods and sixteen warm databases each, at most
48 entries, whatever the repo count. It does not grow with the number of repositories, because a
repo has an entry only while some node holds it open.

## Claiming, renewing, releasing

**Claim.** A node whose local copy shows a repo unowned or expired asks the leader. The leader
either grants it — recording the node and an expiry — or replies with the current owner. Because
one node decides, there is no race to resolve: a second asker is told who won and forwards there.

**Renew.** While a node holds a repo's database open it renews, batched — one message per node per
interval, covering everything it holds. The leader extends the entries that still name the asker
and declines the rest; a declined repo's database is closed at once by the asker. Repos already in
their drain window are left out of the renewal, or renewing would undo the release. Expired entries
are dropped by a separate prune loop on the leader, once per `LEASE_TTL`.

**Release.** When the pool evicts a database — idle past `WARM_TTL`, or pushed out by `MAX_WARM` —
the lease goes with it, in this order:

```
1. tell the leader: expires = now + drain     ← still the owner, still serving
2. keep serving for `drain`                   ← followers whose copy is behind still arrive
3. close the database                         ← nothing points here now
4. the entry lapses; the leader's prune task drops it
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
  cluster, plus one cold open of the ownership database. Repos already open keep serving
  throughout: their holders have the databases and their renewals are advisory. The map survives
  the restart, so nothing has to be rebuilt and no re-claim burst follows. Cold repos claimed
  during the gap get a 503 and the client retries.
* **Pod zero is unreachable from one node** (partition). That node cannot claim; it keeps serving
  what it holds. It does **not** become leader. Other nodes are unaffected.
* **A follower dies.** Its entries expire and its repos are claimed by whoever next serves them.
  Detection is the lease TTL.
* **Everything restarts.** The map persists, and may name owners that no longer hold anything.
  Those entries expire on their own, and a node that is asked for a repo it does not hold consults
  pod zero rather than serving. Stale entries cost a hop, not a wrong open.
* **A stale grant is acted on.** SlateDB's writer epoch fences the second opener, the loser's pool
  reports it and re-routes. Noisy, bounded, never divergent data.

## Why this is safe without consensus

SlateDB's writer epoch is a genuine fencing token: a stale writer's writes are rejected by the
storage layer regardless of what any map says. Correctness has never depended on the ownership
mechanism and does not now. The leader buys *accuracy* — fewer wrong routes, less thrash — and is
worth exactly as much machinery as that is worth. Which is why the map is not replicated, not
run through a consensus protocol: SlateDB's single-writer rule already gives the map exactly the
protection it needs, and the fencing token already gives the repos theirs.

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
third of `proxy.rs`. `KLOUDLITE_REPLICAS` goes too: the leader's name is derived from the pod's
own identity and every other node's address comes from the map, so nothing needs a peer count any
more. Scaling is `spec.replicas` alone.

## What stays

Forwarding, unchanged: HTTP as a reverse proxy, SSH as a byte pipe with a status-line handshake,
both over the secret-guarded peer ports. The hop bound stays as a loop guard. Fencing stays as the
backstop.
