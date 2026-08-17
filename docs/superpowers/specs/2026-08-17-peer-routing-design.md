# Peer routing: the nodes are the load balancer

A repo's database may be open on exactly one node at a time. SlateDB permits one writer per
database, so two nodes serving one repo fence each other. Until now that invariant was kept by
hand — a single pod, or a load balancer configured to hash on the repo path.

This design moves the decision into the nodes. Any node accepts any connection, works out which
node owns the repo, and forwards the request there if it is not the owner. The thing in front can
be a plain round-robin load balancer that understands nothing about git.

Supersedes the ingress-based hash routing, and covers SSH, which no L4 or L7 balancer can route.

## Why routing cannot live in the balancer

Over HTTP the repo is in the path, so an L7 proxy can hash on it. Over SSH it is not: the client
opens a session, authenticates, and only then sends an `exec` request carrying
`git-upload-pack '/owner/name.git'`. That request travels inside the established, encrypted
session. A balancer can route it only by terminating SSH itself — holding the host key,
authenticating the client — which makes the balancer a second place that authenticates, and a
second place to get authentication wrong.

The node already terminates SSH and already parses that exec request. Routing there costs nothing
new.

## Ownership: rendezvous hash, three candidates deep

Each node scores every peer for a repo and ranks them:

    candidates(repo) = peers sorted by fnv1a(repo || peer), descending
    owner(repo)      = first reachable of candidates[0..3]

Properties that matter here:

* **No coordination.** Every node computes the same answer from the same inputs. There is no
  lookup on the request path, no lease to renew, nothing to expire, and no state that can be
  stale or disagree between nodes.
* **Minimal movement.** Adding or removing a peer moves roughly 1/N of repos, not all of them.
  A plain `hash % N` would reshuffle nearly everything on every scaling event.
* **Moving is cheap.** Nothing migrates: the data is in blob storage, and ownership is only which
  node holds the database open. A repo moving costs the new owner one cold open — ~50ms in-region,
  measured — and the old owner one fenced request.

### Membership comes from DNS, not configuration

Peers are resolved from the headless Service (`rustic-git.rustic-git.svc.cluster.local`), cached
for a few seconds, rather than frozen into each pod's environment at startup. A node identifies
itself by its own pod IP, and is always a member of its own peer set regardless of what DNS says:
a pod that is not yet ready is absent from DNS, and without this it would forward every repo it
owns one rank down, then take them all back once ready — one fence per repo on every scale-up.

This is not a nicety. The dangerous state in this design is not handover, it is *disagreement*:
if node B believes A owns a repo while C believes C does, A and C fence each other in turn, one
reopen per flip, failing requests while it flaps. A peer list baked into the environment
guarantees that state for the length of a rolling restart, because pods start with different
lists. Resolving from DNS bounds the disagreement to the cache TTL instead, and scaling needs no
restart at all — change `replicas`, and every node converges within seconds.

Kubernetes publishes only *ready* endpoints in that DNS, so an unready or terminating pod leaves
the candidate set on its own. Most failover therefore costs nothing: the second candidate is
chosen because the first is no longer a candidate, not because a request had to time out first.

Discovery through the Kubernetes API would react faster still, at the cost of RBAC, a Kubernetes
client, and binding the server to running inside Kubernetes. DNS needs none of that and is
already there.

### Failing over past an unreachable candidate

Ready-per-Kubernetes does not mean reachable-from-here, so the top three candidates are tried in
order. The rule that makes this safe:

> **A node may serve a repo only if it cannot itself reach any higher-ranked node.**

Rank is agreed by every node, but reachability is not: node B's inability to reach node A is
B's observation alone. Acting on it directly is what breaks the invariant. Suppose the repo ranks
A, C, B. B cannot reach A, so it forwards to C — while node D, which can reach A, forwards there
too. A and C now both hold the repo, take it from each other in turn, and every takeover costs a
failed request. Ranking is global, reachability is local, and mixing them produces disagreement.

Under the rule, C does not accept the repo on B's word. C sees it is not the top candidate,
checks A from its own vantage point, finds it reachable, and forwards there. A keeps the repo,
C never opens it, and there is no contention to recover from. When A is genuinely down, C cannot
reach it either, agrees with B, and serves — which is the case failover exists for.

Precedence is therefore strictly one-directional: the second candidate serves only while the
first is unreachable *from it*, and the third only while both are. A lower-ranked node never
takes a repo from a higher-ranked one.

Ordinary traffic pays nothing for this: the top candidate serves immediately, having nothing
above it to check. The extra check happens only on the failover path.

Three supporting rules:

* **Reachable means the application answers.** A probe is `GET /healthz` on the peer, not a bare
  TCP connect. A pod mid-shutdown accepts TCP for a moment and then dies; treating that as
  reachable reopens the two-writer window — one node probes it (accepts) and forwards, it dies,
  another probes it (refused) and serves locally. Requiring an HTTP 200 closes most of that
  window. A `preStop` delay on the pod closes the rest: it lets endpoint removal propagate through
  DNS before the pod stops answering, so every node agrees the pod is gone before it goes, and
  shutdown becomes a handover rather than a race.
* **Only connection-level failures fail over** — refused, DNS failure, or probe timeout. An HTTP
  5xx from a peer that answered is that peer's problem to report, not a reason to move a repo.
* **A failed peer is remembered briefly** (a few seconds, in memory) so a node does not retry a
  dead peer on every request, and so consecutive requests agree with each other rather than
  flapping.

Fencing remains the backstop rather than the mechanism. If two nodes do both open a repo — during
a scale, or a partition that splits the fleet's views — the second takes the writer epoch and the
first's next write fails cleanly. Safe, but it costs a request, which is why the rule above exists
to make it rare rather than routine.

## Data flow

    HTTP client ─▶ LB ─▶ node B ─┬─ owner? ──────▶ serve locally
                                 └─ not owner ───▶ proxy to node A ─▶ stream back

    SSH client ─▶ LB ─▶ node B: terminate SSH, authenticate the key,
                                parse exec 'git-upload-pack /o/r.git'
                                ├─ owner? ──────▶ serve locally
                                └─ not owner ───▶ open peer stream to node A,
                                                  send one header line,
                                                  then pipe bytes both ways

The two edges do not share an internal protocol, and trying to make them was a mistake worth
recording. Over HTTP, protocol v2 is one command per POST with the advertisement served by
`GET info/refs`. Over SSH it is a *session*: one advertisement followed by repeated
`command=ls-refs` / `command=fetch` exchanges on a single stream, which is what the loop in
`protocol/upload.rs` implements. One SSH session is therefore N HTTP requests, and translating
between them means writing a v2 session splitter that must know exactly where each command ends.

Instead, SSH forwards as a byte pipe. Node B opens a connection to node A's peer stream listener,
sends a single header line, and then copies bytes in both directions. Node A hands that socket to
the same `upload::serve` / `receive::serve` it would call for a local SSH client, so the owner's
protocol handling is byte-for-byte what it is today and there is no translation layer to get
subtly wrong.

    <secret> <service> <owner>/<name> <authenticated-owner> <hops>\n   then raw git protocol both ways

HTTP forwarding stays a plain reverse proxy: same method, path, headers and body to the peer,
response streamed back.

## Peer authentication

A forwarded request must tell the owner who authenticated, because the credential was checked at
the edge and is not re-presented.

Peer traffic gets **its own listeners on their own ports**, published by no Service and reachable
only from inside the cluster network:

* `RUSTIC_GIT_PEER_ADDR` (default `0.0.0.0:8081`) — HTTP, for forwarded HTTP client requests.
* One port above it (`8082`) — the byte pipe, for forwarded SSH sessions. Derived, not configured:
  peers are addressed by their HTTP peer port everywhere else, and a second list would be a second
  thing to keep in agreement.

The public listeners on 8080 and 2222 never honour an identity claim at all.

    X-Rustic-Git-Owner: <authenticated owner>   # honoured on the peer HTTP listener only
    <secret> <service> <repo> <owner> <hops>\n  # the peer stream's first line

Trust is positional first: a request that arrived on the peer port came from inside the cluster,
and only nodes are told that port exists. A separate socket cannot be reached by a client at all,
and the failure mode of forgetting to publish a port is an outage, not a breach — the direction
you want a mistake to fall.

It is not positional *only*. Pod networking is flat, so anything else running in the cluster can
reach the peer ports, and the natural fix — a NetworkPolicy restricting them to `rustic-git` pods
— is silently ignored on this cluster: `kolomi-cluster` was created with `networkPolicy: none`,
so the policy object is accepted and enforces nothing. So every forwarded request also carries a
shared secret (`X-Rustic-Git-Peer`, and the first token of the stream header) from a Kubernetes
Secret, checked on the peer listeners only. Wrong or missing, the request is refused before
anything else is read.

This is defence in depth on top of the separate port, not a replacement for it. A secret checked
on the *public* socket would make one string the whole boundary; a secret checked on a socket a
client cannot reach in the first place is a second wall behind the first. The NetworkPolicy is
kept in the manifests for a cluster that does enforce one.

The public listener also strips the hop-count header. A client that could set it to the maximum
would force any node to serve a repo it does not own — opening it and fencing the real owner —
which is an unauthenticated way to disrupt any repo.

A forwarded request may be forwarded once more, because the receiving node re-checks the nodes
ranked above it and may find one reachable that the sender could not. Chains are bounded by a hop
count carried with the request — at most two hops, since candidates are only three deep — rather
than by trusting the routing to converge. A request that has exhausted its hops is served where it
lands: being wrong about ownership costs one fenced request, while bouncing forever costs the
client everything.

## Failure handling

* **Peer unreachable.** Try the next candidate, up to three deep, and let that candidate confirm
  the verdict from its own vantage point before serving. Only connection-level failures count; an
  HTTP error from a peer that answered is returned to the client as-is. A peer that failed to
  connect is skipped for a few seconds so consecutive requests agree rather than flap.
* **All three candidates unreachable.** Return 503 with a plain message. git retries, and
  Kubernetes restarts the pods.
* **Fenced handle.** Already handled in `Pool::get`: a closed database is dropped and reopened.
* **Client disconnects.** Unchanged — work is cancelled when the client goes away. The forwarding
  node must propagate cancellation to the owner rather than leaving an orphaned request.

## Components

| Unit | Responsibility | Depends on |
|---|---|---|
| `peers::rank` | `candidates(repo, peers) -> ordered peers`; this node's own rank for a repo | nothing |
| `peers::Membership` | Resolve the headless Service, cache briefly, track recent connect failures | DNS resolver |
| `proxy::http` | Reverse-proxy one request to a peer, streaming the response, propagating cancellation | HTTP client |
| `proxy::stream` | Dial a peer's stream port, write the header line, copy bytes both ways | tokio |
| `http` | Owner-or-forward before handling; peer listener honours the identity header | `peers`, `proxy` |
| `ssh` | After exec parsing, serve locally or translate into a peer call | `peers`, `proxy` |

`peers::rank` is pure computation, testable with no I/O and no network — the property that matters
most, since every node agreeing is the correctness condition. Splitting it from `Membership` keeps
the DNS cache and failure tracking (which need a clock and a resolver) out of the part that must
be provably deterministic.

## Testing

* **Rendezvous hash.** Determinism; every node computes the same ranking for the same repo;
  removing a peer moves roughly 1/N of repos and leaves the rest untouched; the second candidate
  of the full set equals the first candidate of the set with the winner removed — the property
  failover depends on.
* **Forwarding.** Two servers in one process over an in-memory store. A request to the non-owner
  is served correctly, and the *owner* is the node that opened the database — asserted through the
  pool's warm count, so the test fails if both nodes open it.
* **Peer auth.** `X-Rustic-Git-Owner` sent to the *public* listener is ignored and the request is
  authenticated normally — the test that matters, since it is the bypass a client would attempt.
  The same header on the peer listener is honoured, and that request is served locally regardless
  of the hash.
* **Failover.** With the first candidate refusing connections, the request is served by the second;
  with an HTTP 500 from the first, the error is returned rather than failed over.
* **Precedence.** A node that is not the top candidate, sent a request it did not rank for, forwards
  it to the top candidate rather than serving it — the rule that stops two nodes holding one repo.
  With every higher-ranked node unreachable, it serves.
* **Hop bound.** A request that has been forwarded twice is served where it lands rather than
  forwarded again.
* **SSH forwarding.** An SSH clone of a repo owned by another node returns the same bytes as a
  local one, including a multi-command session (`ls-refs` followed by `fetch`) on one connection —
  the case a single-request translation would have broken.
* **Peer stream trust.** A header line naming an owner is honoured on the stream port; the public
  SSH port has no such input at all. A wrong secret on the stream port is closed without a byte.
* **Peer secret.** The peer HTTP listener refuses a request with a missing or wrong secret before
  reading anything else. The public listener strips the secret, identity and hop-count headers.
* **Reachability.** A listener that accepts TCP and closes without answering HTTP is *not*
  reachable.
* **Rolling restart.** A clone loop against the load balancer during `rollout restart` sees no
  failures — the `preStop` delay is what this proves.
* **Real transport.** A real `git push` and `git clone` through a forwarding node, over HTTP and
  over SSH, produce correct results and leave exactly one node's pool warm.

## Deployment changes

Removed: the Ingress and its `upstream-hash-by` annotations, and the `rustic-git-http` Service.

Added: a `LoadBalancer` Service publishing 80 and 2222 across all pods; `RUSTIC_GIT_PEER_DNS`,
`RUSTIC_GIT_SELF_IP` and `RUSTIC_GIT_PEER_SECRET` (from a Secret) on the StatefulSet with the
peer container ports, published by no Service; a `preStop` sleep so a terminating pod leaves DNS
before it stops answering; and a NetworkPolicy allowing 8081 and 8082 only from pods labelled
`app: rustic-git`, kept for a cluster that enforces one.

No peer list: membership is the headless Service's DNS. Scaling is `kubectl scale` with no
restart and no config edit.

## Not in scope

**Serving object traffic from any node.** Reads and writes of pack files could in principle run
anywhere — packs are immutable and every node reads them straight from blob storage — but today
every git route touches the refs database: `upload-pack` computes reachable tips
(`protocol/upload.rs`) and the pack index lives in the same database. Freeing clone traffic from
the owner needs a read-only ref view, which is a separate design with its own staleness
trade-offs. Clone bandwidth is the reason to want it, so it is likely the next piece of work.

**Ownership that survives scaling.** Rendezvous hashing already moves the minimum a stateless
scheme can — about 1/N of repos — but it does move some. Zero movement needs ownership state: a
node writes a lease while it holds a repo, and peers honour a live lease over their own ranking,
so scaling redistributes only idle repos and leaves active ones alone. The costs are a lookup on
the routing path, renewal while active, and a stale lease pointing at a dead node until it
expires — the claim/renew machinery deleted in `1a558f9`.

Deliberately deferred, because the thing it optimises is already cheap: a repo moving costs one
cold open (~50ms in-region), not a data migration. Revisit if scaling events prove disruptive in
practice, which will show up as failed in-flight pushes during a scale, not as slowness.
