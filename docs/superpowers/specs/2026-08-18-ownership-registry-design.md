# Ownership registry: one map in etcd, not a rule in every node's head

A repo's database may be open on exactly one node. Today every node works that out for itself:
rank the peers by rendezvous hash, probe the ones above you, ask a second peer to probe them too,
and serve only if everyone agrees the higher ranks are unreachable. It is correct, and it is a lot
of machinery to answer a question that could simply be *written down*.

This design writes it down. A single Kubernetes ConfigMap holds `repo → (node, expires)`. Every
node watches it, so every node knows who owns what within milliseconds. A node that receives a
request for a repo with a live entry forwards there. A node that receives one for a repo with no
entry — or an expired one — claims it with a compare-and-swap and serves it.

Supersedes the peer-routing design's rank-and-probe rule. Keeps its forwarding (HTTP reverse
proxy, SSH byte pipe), its peer ports and secret, and fencing as the backstop.

## Why replace a working design

The rank-and-probe rule works, and it is the second thing this system has tried. What both
attempts share is that they *derive* ownership from something else — a hash of the peer list, and
before that a hash in a load balancer — and then spend their complexity reconciling that
derivation with reality.

Deriving ownership means the derivation's inputs must be perfect. The peer list came from DNS, and
a deploy proved DNS is a cache: it went stale during exactly the events that matter, dropped a
restarting pod from the set so its repos were re-ranked while it still held them, and returned an
IP-derived name for a pod that had a second Service pointing at it. Each was fixed. The
replacement — a static peer set from the StatefulSet's identity — removed that class entirely, at
the cost of no longer knowing which peers are *alive*, which is what the probing is for.

So: rank from config, liveness from probes, and a two-phase rule to keep two nodes from acting on
different views of liveness. Measured on a rolling restart, a failover cost about nine seconds of
probing, and clients gave up long before that.

An ownership map inverts it. Ownership is not derived from anything; it is a fact one node wrote
and every node reads. Liveness stops being a question asked per request and becomes a timestamp.
The two-phase vantage rule, the probes and their timeouts and caches, the hop counting and the
rank-based failover all become one lookup.

## Why Kubernetes, and why not a Lease

The registry needs atomic claim: two nodes must not both take an unowned repo. That is a
compare-and-swap, and where it lives decides how reliable the answer is.

Blob storage gives CAS through `If-Match`, which Azure honours. It would need no new dependency.
It has two weaknesses: expiry is compared against each node's own clock, and readers must poll,
so every node's view is as stale as its poll interval.

etcd — through the Kubernetes API — gives the same CAS through `resourceVersion`, and two things
blob storage cannot. It is a consensus system, so the CAS is linearizable rather than
best-effort-per-object. And it supports **watch**: a change is pushed to every node in
milliseconds instead of discovered on the next poll. Propagation delay is the thing that made
every previous handover ugly, and watch is the only mechanism here that actually removes it.

The natural object would be `coordination.k8s.io/v1 Lease`, which exists for exactly this. It
holds one `holderIdentity`, so per-repo ownership means one Lease object per repo. Two problems:
Leases do not disappear when they expire, so the cluster accumulates one object per repo ever
touched; and each node must renew every repo it holds separately, one write each.

A single ConfigMap holding the whole map fixes both. Renewal batches — one write per node per
interval, covering every repo that node owns, however many that is. And a node writing its
renewal prunes entries that have expired, so the map stays the size of the working set rather than
the repo count.

The cost of choosing Kubernetes at all: the `kube` and `k8s-openapi` crates, a ServiceAccount with
a Role over one ConfigMap, and a server that now requires Kubernetes to run multi-node. The
single-node path keeps working with no registry at all — one node owns everything, which is true
by construction.

## The registry

One ConfigMap, `rustic-git-ownership`, in the server's namespace. One key, `map`, holding JSON:

    { "alice/web": { "node": "rustic-git-1", "expires": "2026-08-18T09:14:03Z" }, ... }

* **`node`** is the pod name, which is stable across restarts. It is resolved to an address the
  same way peers are today — `<node>.<headless-svc>:<peer-port>` — at connect time.
* **`expires`** is absolute UTC. A node renews well before it, so an entry is only expired if its
  holder stopped renewing: it crashed, was killed, lost the API server, or released the repo.

Nodes hold the map in memory from a **watch**, so the routing path never calls the API server. The
watch is the only reader; a full re-list happens on watch failure or restart.

## Reading: where a request goes

    entry missing or expired  →  claim it (below); on success serve, on loss re-read and forward
    entry names another node  →  forward there (HTTP proxy / SSH pipe, unchanged)
    entry names this node     →  serve

There is no probing, no ranking, and no failover decision. A node that has crashed stops renewing;
its entries expire; the next request for those repos claims them. Detection time is the lease TTL,
which is a number in a config file rather than an emergent property of probe timeouts.

## Claiming

    read map (from the watch cache)
    entry is absent or expired?
        build the new map with this node and expires = now + ttl
        PUT ConfigMap with resourceVersion = the version this map came from
            409 Conflict  → someone else wrote first: re-read, start over
            200 OK        → we own it: open the database and serve

The `resourceVersion` precondition is what makes the claim atomic; without it two nodes reading
the same expired entry would both write, and both would open the database. Since a lost claim is
resolved by re-reading and forwarding, the loser costs one extra round trip, not an error.

**Renewal** is the same write, batched: every `ttl / 3`, a node CASes the map once, extending every
entry it holds and dropping every entry that has expired. One write per node per interval,
independent of how many repos it owns.

## Releasing, and the ordering that matters

A node releases a repo when it evicts the database (idle) or when it is shutting down. The order
is the part that has already been got wrong once in this system, on a real cluster:

    1. CAS the map: expires = now + drain          ← still the owner; keep serving
    2. keep serving for `drain`                    ← nodes whose watch has not yet caught up still arrive
    3. close the database                          ← nothing points here now
    4. the entry expires on its own; the next renewal prunes it

**Do not delete the entry.** Deleting makes the repo claimable immediately, so another node can
open the database while this one is still draining — and that open fences it. That is exactly the
failure this system produced when a terminating pod left the Service's endpoints before releasing
its repos: peers concluded the repos were ownerless, opened them, and fenced a pod that was still
serving. Shortening the expiry keeps the lease valid, which keeps claimants out, while announcing
when it stops being valid.

`drain` must exceed watch propagation plus in-flight request time. Watch delivers in milliseconds,
so a few hundred is generous; the previous poll-based design needed seconds.

**The database must be closed before the entry becomes claimable.** Invert steps 2 and 3 and the
fence is back.

## What stays

**Fencing remains the backstop, and it is what makes the registry advisory rather than critical.**
SlateDB's writer epoch is a real fencing token: a stale writer's writes are rejected by the storage
layer regardless of what any registry says. So a registry error costs a failed request and a
reopen, never divergent data. This is why etcd's reliability is a convenience here and not a
dependency — the correctness guarantee is already underneath.

Forwarding is unchanged: HTTP as a reverse proxy, SSH as a byte pipe with a status-line handshake,
both over the secret-guarded peer ports. The hop bound stays as a loop guard.

## What goes

`Membership`, `rank`, the two-phase `decide`, `/probe` and `probe_via`, probe timeouts, retries,
positive caching and single-flight, the hop-based failover, and `RUSTIC_GIT_REPLICAS`. Roughly two
thirds of `peers.rs` and a third of `proxy.rs`.

## Failure modes

* **API server unreachable.** The watch breaks; nodes keep serving from their last known map and
  retry the watch. Claims fail, so a repo whose owner died stays unavailable until the API server
  returns. This is the trade for centralising ownership, and it is bounded: an unreachable API
  server does not disturb repos whose owners are alive and renewing.
* **A node cannot renew** (API server partition, pause). Its entries expire, another node claims
  them, and its next write is fenced. The pool reports the fence and re-routes, as it does now.
* **Clock skew** shifts expiry judgements. Absolute timestamps compared against local clocks
  assume NTP-synced nodes, which Kubernetes nodes are; a TTL of seconds absorbs milliseconds of
  skew, and fencing absorbs the rest.
* **ConfigMap size.** One entry is roughly sixty bytes; the 1 MiB limit is some seventeen thousand
  concurrently-held repos. Since only open repos hold entries and `RUSTIC_GIT_MAX_WARM` bounds
  those per node, the practical ceiling is far higher than the fleet will reach. Shard by hash
  prefix if it ever does.
* **Write contention.** Every node CASes one object. At three nodes renewing every few seconds this
  is nothing; conflicts retry. Past a few dozen nodes, shard the map.

## Migration

The registry replaces routing, not storage, so the two can run side by side: build it behind a
flag, verify it under the same roll-under-load test the current design passes, and remove the old
path once it does. No data moves; a repo's database is where it always was.
