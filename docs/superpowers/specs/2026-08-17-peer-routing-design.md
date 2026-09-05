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

    score(repo, peer) = mix(fnv1a(repo), fnv1a(peer-name))   # independent hashes, finalized
    candidates(repo)  = peers sorted by score, descending
    owner(repo)       = first confirmed-reachable of candidates[0..3]

The two inputs are hashed separately and mixed through a finalizer rather than concatenated into
one FNV-1a pass. Concatenation looked right and was reviewed six times, but FNV's last-byte
sensitivity let the peer-name suffix dominate the top bits: measured over 30 000 repos at three
nodes, one node owned exactly half of them. A distribution test caught what reading did not.

The hash key is the peer's **stable name** (`kloudlite-0`, `kloudlite-1`, …), never its IP. A
StatefulSet pod keeps its name across restarts but gets a new IP each time, so hashing on IP would
make every restarted pod a *new* peer: over one rolling restart nearly every repo would move at
least twice. Hashing on the name means a restart moves nothing — the same pod comes back owning
the same repos.

Properties that matter here:

* **No coordination.** Every node computes the same answer from the same inputs. There is no
  lookup on the request path, no lease to renew, nothing to expire, and no state that can be
  stale or disagree between nodes.
* **Minimal movement.** Adding or removing a peer moves roughly 1/N of repos, not all of them.
  A plain `hash % N` would reshuffle nearly everything on every scaling event.
* **Moving is cheap.** Nothing migrates: the data is in blob storage, and ownership is only which
  node holds the database open. A repo moving costs the new owner one cold open — ~50ms in-region,
  measured — and the old owner one fenced request.

### Membership is the StatefulSet's identity

A StatefulSet behind a headless Service already names its members: `replicas: N` means the peers
are `{app}-0 … {app}-{N-1}`, and those names survive every restart, reschedule and IP change. So
membership is configuration — `KLOUDLITE_SELF` (downward API) gives the app name and this node's
hash key, `KLOUDLITE_REPLICAS` gives the count, and each peer's address is
`{app}-{i}.{headless-svc}:{peer-port}`, a hostname the OS resolves when a connection is actually
made. Nothing is polled, cached, or able to go stale. The earlier design resolved the Service's A
records every couple of seconds, which conflated membership with readiness: a restarting pod left
the endpoint list and therefore left the peer set, so the fleet re-ranked its repos and opened them
while it still held them — fencing it, one burst of 503s per roll. Liveness is a separate question
with its own answer: peers probe `/healthz` on the peer port, and a member that is briefly
unreachable is handled by the two-vantage rule below (refuse, do not take over). Scaling now means
changing `replicas` and `KLOUDLITE_REPLICAS` together and rolling — the cost of never being wrong
about who the members are.

### Failing over past an unreachable candidate

Ready-per-Kubernetes does not mean reachable-from-here, so the top three candidates are tried in
order. The rule:

> **A node may serve a repo only if every higher-ranked node is unreachable from at least two
> vantage points: its own, and one other reachable peer's.**

Rank is agreed by every node, but reachability is not: node C's inability to reach node A is C's
observation alone. Acting on a single local observation is what breaks the invariant, in two ways.

*Hearsay.* Repo ranks A, C, B. B cannot reach A, so it forwards to C — while node D, which can
reach A, forwards there too. If C serves because B sent it the request, A and C both hold the
repo. So a node never serves on another node's word: it re-checks the nodes above it from its own
vantage point and forwards up if one answers.

*One-sided partition.* Same ranking, but now the link C↔A is cut while every other link works.
Requests landing on B reach A. Requests landing on C: C probes A, fails, and — if its own view
were enough — serves. A and C hold the repo for as long as the partition lasts. Nothing C can
observe by itself distinguishes "A is down" from "I cannot see A". That distinction needs a second
vantage point.

The rule has two phases, and the split matters. *Forwarding up needs no vantage*: a node probes
every higher-ranked candidate itself — concurrently, so a blackholed pod does not stack timeouts —
and forwards to the best that answers. That alone defeats hearsay. *Serving needs a vantage on
every node above*: only if nothing above answers does the node ask other peers to probe each
higher-ranked node on its behalf (`GET /probe?peer=<name>` on the peer listener). Any peer that is
not the node itself and not the target may vouch, lower-ranked candidates included — in a
three-node fleet the third candidate has nobody else. Vouching only for the top-ranked node is not
enough: with the owner dead and the second candidate merely cut off from *this* node, serving on a
vantage about the owner alone puts this node and the second both on the repo.

Evidence is asymmetric. A vantage that *reaches* the target has a 200 from it — hard proof it is
alive — and any such answer, from any vantage, about any target, vetoes serving: this node is the
one cut off. A vantage that cannot reach it has a timeout — soft — and serving needs one for every
node above. A target nobody could vouch about is a split fleet as far as that target goes:
Unavailable. And a higher-ranked node that answers a vantage request at all has just proven it is
reachable from here after all; the node forwards to it rather than serve past it. In the one-sided partition, C asks B, B reaches A —
so C is the one cut off, and C returns 503 rather than forward to an address it just failed to
reach; the client retries and round robin lands it elsewhere. When A is genuinely down, B cannot
reach it either, C serves.

Two independent vantage points agreeing that a node is unreachable is not proof it is down. It
removes the class of single-link failures, which is what one-sided partitions are. It does **not**
remove *correlated* failure: an owner that is alive but slow — a GC pause, a saturated runtime —
can time out from two peers for one cause, and the owner, as top candidate, never verifies that
anyone can reach it. The same holds for a lower-ranked node serving under a confirmed outage: it
never verifies that anyone can reach *it*, so a vantage that cannot — a one-directional cut, or a
timeout while it is briefly slow — lets the next rank in beside it. Both are the same shape: the
acting owner is unchecked. What follows is fencing, then the fenced node's own routing still says
`Local`, then it reopens — a ping-pong until the cut heals, with no data lost. So probes are generous (seconds, not hundreds of milliseconds) and retried
once, a positive probe is cached briefly so a hot owner is probed once per second per node rather
than once per request, and the deployment spreads candidates across physical nodes so two vantages
are not one link. What remains is the backstop: fencing, which turns the residual case into a
failed request rather than lost data. This limit is stated in the README.

The trade, stated plainly: this is **safety over availability**. If a node cannot find a second
vantage — every other peer is unreachable too — it returns 503 rather than serve. That is a
partition splitting the fleet, and serving through it is how two writers happen. The client
retries; SlateDB fencing remains the backstop for the cases two vantages do not catch, and it
guarantees no data is lost, only that a request fails.

Precedence is strictly one-directional: the second candidate serves only while the first is
confirmed unreachable, and the third only while both are. A lower-ranked node never takes a repo
from a higher-ranked one on one node's word, including its own.

Ordinary traffic pays nothing for this: the top candidate serves immediately, having nothing
above it to check. The probe and the second-vantage probe happen only on the failover path.

Supporting rules:

* **Reachable means the application answers.** A probe is `GET /healthz` on the peer, not a bare
  TCP connect. A pod mid-shutdown accepts TCP for a moment and then dies; treating that as
  reachable reopens the two-writer window — one node probes it (accepts) and forwards, it dies,
  another probes it (refused) and serves locally. Requiring an HTTP 200 closes most of that
  window. A `preStop` delay on the pod closes the rest: it lets endpoint removal propagate through
  DNS before the pod stops answering, so every node agrees the pod is gone before it goes, and
  shutdown becomes a handover rather than a race.
* **Only probe failures fail over.** A probe is a `GET /healthz`; only a probe that does not
  return 200 counts a peer as unreachable. A forward that fails *after* the probe succeeded — the
  client aborted its upload, the peer returned 5xx, the peer closed mid-stream — is returned to
  the client and does **not** mark the peer down. Otherwise an unauthenticated client could push
  half a body to a non-owner, abort, and thereby demote the owner: routing runs before
  authentication, so anything a forward error can trigger, anyone can trigger.
* **Negative probes are never remembered.** A positive probe is cached for a second so a hot owner
  is not probed per request; a negative one is not cached at all, because a stale "down" is a stale
  reason to demote a healthy peer. Serving as a non-top candidate always rests on a fresh probe and
  a fresh second vantage.
* **The second-vantage request waits longer than the vantage's own probe.** The vantage answers by
  probing the target itself, with retry; if the asker's timeout is shorter, every answer on a
  genuinely dead owner is "could not ask" and failover never happens.
* **An unhealthy node never serves, but still forwards.** Its peers see its `/healthz` fail and
  will take its repos; if it kept serving them, that is two writers. It answers 503 for what it
  would have served and forwards what it does not own — forwarding is safe, and keeps its share of
  load-balancer traffic flowing. Health has hysteresis — three consecutive failures — so one slow
  object-store round trip does not flip every node at once.
* **At the hop limit, routing is still consulted.** A request that has been forwarded the maximum
  number of times is never forwarded again — that is the bound — but it is served only if this
  node's own routing says so. A chain that arrives disagreeing with the local view, or at an
  unhealthy node, gets 503 rather than a knowing wrong open.
* **Probes are single-flight per address.** Concurrent requests for a dead owner's repos share one
  in-flight probe rather than each issuing their own; negatives are still never cached.
* **Candidates are the top three by rank, then filtered** — never "the top three that are up".
  Otherwise ranks four and five become owners the moment one and two are down, and the fleet no
  longer agrees on who the candidates are.
* **`/healthz` must mean healthy.** Reachability and Kubernetes liveness both key off it, so a
  node whose object-store connection has died must fail it, or it keeps its repos and returns 500
  to every client indefinitely with no failover and no restart. It reports the result of a recent
  object-store round trip: healthy if the store *answered* — OK, or not-found for the probe key —
  unhealthy on transport failure, and on authentication failure too, since a rotated storage key
  is exactly the "keeps its repos and 500s forever" case.

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

    <secret> <service> <owner>/<name> <authenticated-owner> <hops>\n
    ← "ok\n"  or  "error: <reason>\n" then close          then raw git protocol both ways

On a two-hop forward the middle node reads the owner's status line and relays it upstream before
piping; without that, the edge node reads the first git packet as its status.

The owner answers the header with one status line before any git bytes. Without it, an
authorisation refusal on the owner is indistinguishable, at the forwarding node, from a clean end
of session: the client would see exit status 0 and empty output where a local session prints the
reason. `ok` is sent after the header is validated and authorised but *before* the repo is opened:
opening a cold repo downloads its packs, and the forwarding node is waiting on this line under a
timeout sized for a header exchange. A missing repo is reported after `ok`, on git's own `ERR`
channel with a non-zero exit — the same way a local session reports it. Every field is validated on the owner — `service` must be one of the
two git services, `owner` and repo segments must pass `valid_segment` — the line is capped and
read under a timeout so a stray connection cannot hold a task, and a hop count that fails to parse
is treated as exhausted (serve here) rather than fresh (forward again).

HTTP forwarding stays a plain reverse proxy: same method, path, headers and body to the peer,
response streamed back.

## Peer authentication

A forwarded request must tell the owner who authenticated, because the credential was checked at
the edge and is not re-presented.

Peer traffic gets **its own listeners on their own ports**, published by no Service and reachable
only from inside the cluster network:

* `KLOUDLITE_PEER_ADDR` (default `0.0.0.0:8081`) — HTTP, for forwarded HTTP client requests.
* One port above it (`8082`) — the byte pipe, for forwarded SSH sessions. Derived, not configured:
  peers are addressed by their HTTP peer port everywhere else, and a second list would be a second
  thing to keep in agreement.

The public listeners on 8080 and 2222 never honour an identity claim at all.

    X-Kloudlite-Owner: <authenticated owner>   # honoured on the peer HTTP listener only
    <secret> <service> <repo> <owner> <hops>\n  # the peer stream's first line

Trust is positional first: a request that arrived on the peer port came from inside the cluster,
and only nodes are told that port exists. A separate socket cannot be reached by a client at all,
and the failure mode of forgetting to publish a port is an outage, not a breach — the direction
you want a mistake to fall.

It is not positional *only*. Pod networking is flat, so anything else running in the cluster can
reach the peer ports, and the natural fix — a NetworkPolicy restricting them to `kloudlite` pods
— is silently ignored on this cluster: `kolomi-cluster` was created with `networkPolicy: none`,
so the policy object is accepted and enforces nothing. So every forwarded request also carries a
shared secret (`X-Kloudlite-Peer`, and the first token of the stream header) from a Kubernetes
Secret, checked on the peer listeners only. Wrong or missing, the request is refused before
anything else is read.

This is defence in depth on top of the separate port, not a replacement for it. A secret checked
on the *public* socket would make one string the whole boundary; a secret checked on a socket a
client cannot reach in the first place is a second wall behind the first. The NetworkPolicy is
kept in the manifests for a cluster that does enforce one.

The public listener also strips the hop-count header. A client that could set it to the maximum
would force any node to serve a repo it does not own — opening it and fencing the real owner —
which is an unauthenticated way to disrupt any repo.

The routing middleware and the request handlers must agree on what the repo *is*. The middleware
reads the raw URI; the handlers read the framework's percent-decoded path. A name that fails to
parse at the middleware must be refused there, not passed through as "not a git route" — otherwise
the handler decodes it and opens the repo locally on whichever node the balancer picked, bypassing
routing entirely. A git-shaped path whose repo does not parse is a 400 before any handler runs.

The peer listener serves two things besides forwarded git requests, both under the same secret:
`GET /healthz` (so probes can tell whether the *application* answers, not merely whether the
kernel accepts a connection — a pod mid-shutdown does the latter for a moment) and
`GET /probe?peer=<name>` (the second vantage: "can *you* reach this peer?"). Probes carry the
secret; a probe refused for lacking it would read as "unreachable", and every peer would then look
down to everyone — routing would silently collapse to every node serving everything.

Forwarding strips hop-by-hop headers in both directions — `Connection`, `Transfer-Encoding`,
`Expect`, `Content-Length` — and lets each hop frame its own body. git sends `Expect:
100-continue` on pushes over 1 MiB, and forwarding it verbatim to a peer that then answers with
its own framing is a mismatch that a one-file test push never exercises.

A forwarded request may be forwarded once more, because the receiving node re-checks the nodes
ranked above it and may find one reachable that the sender could not. Chains are bounded by a hop
count carried with the request — at most two hops, since candidates are only three deep — rather
than by trusting the routing to converge. A request that has exhausted its hops is never forwarded
again; it is served if the local routing decision is `Local`, and refused with 503 otherwise —
bouncing forever costs the client everything, but so does a knowing wrong open.

## Failure handling

* **Peer unreachable.** Try the next candidate, up to three deep. That candidate confirms the
  verdict from its own vantage point *and* one other peer's before serving; if it cannot get a
  second vantage it returns 503. Only probe failures count; an HTTP error from a peer that
  answered is returned to the client as-is.
* **All three candidates unreachable.** Return 503 with a plain message. git retries, and
  Kubernetes restarts the pods.
* **Fenced handle.** Under routing, "I got fenced" almost always means "another node believes it
  owns this repo". Blindly reopening — which `Pool::get` does today — takes it straight back and is
  the amplifier that turns any disagreement into a flap. `Pool::get` therefore evicts a fenced
  handle and *reports* it rather than reopening; every place a fence can surface (HTTP open, the
  protocol handlers, SSH, the peer stream) answers 503 "retry", and the retry re-enters routing,
  which reopens only if this node is still `Local` — in-handler for HTTP, whose body is already
  buffered, since git does not retry a 503 by itself. The pool never reopens a fenced handle on
  its own, so the `Local` path evicts it explicitly before the retry opens fresh. Fences also surface mid-request inside the
  protocol handlers, and `receive-pack` today swallows every apply error into a per-ref `ng` line,
  so a fence during a push must be propagated rather than reported as a failed ref.
* **Shutdown.** The process traps SIGTERM, stops accepting on the public HTTP listener, drains its
  in-flight requests, and closes every warm database. Without this, Kubernetes' SIGTERM kills the
  process outright: in-flight clones and pushes on that pod die, `pool.close()` never runs, and
  the next opener replays the WAL. The `terminationGracePeriodSeconds` is meaningless without a
  handler that uses it. Both HTTP listeners drain — for repos this node owns, most traffic
  arrives on the peer listener; SSH sessions still in flight are cut when the drain ends. The `preStop` delay makes that rare — the
  pod has left every node's DNS before it stops — and it is stated in the README as a limit.
* **Admin commands** open repo databases from a second process and therefore fence the pod that
  serves them. They are run against a drained pod, or routed through the owner; running them
  against a live fleet is a fence per repo touched.
* **Client disconnects.** Unchanged — work is cancelled when the client goes away. The forwarding
  node must propagate cancellation to the owner rather than leaving an orphaned request.

## Components

| Unit | Responsibility | Depends on |
|---|---|---|
| `peers::rank` | `rank(repo, names) -> ordered names`; pure | nothing |
| `peers::Membership` | Resolve the headless Service → (name, ip:port), cache briefly; `decide()` implements the two-phase rule with probe and second-vantage closures passed in | DNS resolver |
| `proxy::Forwarder` | `reachable(peer)` = healthz 200 with secret, retried, positive-cached, single-flight; `probe_via(peer, target)` = second vantage; `forward()` = reverse proxy with hop-by-hop headers stripped | HTTP client |
| `proxy::stream` | Peer stream listener (validate, status line, hand to `serve_git`) and `stream_to_peer` (dial, header, wait for status, copy) | tokio |
| `http` | Public router: strip routing headers, then route. Peer router: check secret, serve `/healthz` and `/probe`, then route again (bounded by hops), honour identity | `peers`, `proxy` |
| `ssh` | After exec parsing, route; serve locally or pipe to the owner, keeping the channel alive across exit status | `peers`, `proxy` |

`peers::rank` is pure computation, testable with no I/O and no network — the property that matters
most, since every node agreeing is the correctness condition. `decide()` takes its probe and
second-vantage functions as parameters, so the rule itself is tested with no network either: every
scenario in the failover section (hearsay, one-sided partition, genuine outage, no second vantage)
is a unit test with scripted reachability.

## Testing

* **Rendezvous hash.** Determinism; every node computes the same ranking for the same repo;
  removing a peer moves roughly 1/N of repos and leaves the rest untouched; the second candidate
  of the full set equals the first candidate of the set with the winner removed — the property
  failover depends on.
* **Forwarding.** Two servers in one process over an in-memory store. A request to the non-owner
  is served correctly, and the *owner* is the node that opened the database — asserted through the
  pool's warm count, so the test fails if both nodes open it.
* **Peer auth.** `X-Kloudlite-Owner` sent to the *public* listener is ignored and the request is
  authenticated normally — the test that matters, since it is the bypass a client would attempt.
  The same header on the peer listener is honoured, and that request is served locally regardless
  of the hash.
* **Failover.** With the first candidate refusing connections, the request is served by the second;
  with an HTTP 500 from the first, the error is returned rather than failed over.
* **Precedence, as unit tests on `decide()` with scripted reachability:** top candidate serves
  without probing; second forwards up when the first answers; second serves only when the first
  fails *both* its own probe and the second vantage; second returns 503 when the first fails its
  own probe but the second vantage reaches it (the one-sided partition); second returns 503 when
  no other peer is reachable to ask; a node outside the top three never serves; a "down" memory
  entry skips a forward but never by itself promotes to serve.
* **Precedence, end to end:** a node sent a forwarded request for a repo whose owner it can reach
  forwards it there, and only the owner's pool opens the repo — asserted with one `Store` per node.
* **Hop bound.** A request that has been forwarded twice is served where it lands rather than
  forwarded again.
* **SSH forwarding.** An SSH clone of a repo owned by another node returns the same bytes as a
  local one, including a multi-command session (`ls-refs` followed by `fetch`) on one connection —
  the case a single-request translation would have broken.
* **Peer stream trust.** A header line naming an owner is honoured on the stream port; the public
  SSH port has no such input at all. A wrong secret is closed without a byte; an unauthorised
  owner or a missing repo gets an `error:` status line, which the forwarding node relays and turns
  into a non-zero exit status.
* **Peer stream parsing.** Over-long header, no newline within the timeout, unknown service, an
  owner with a space, and an unparseable hop count are each refused or served-here, never
  forwarded.
* **Peer secret.** The peer HTTP listener refuses a request with a missing or wrong secret before
  reading anything else. The public listener strips the secret, identity and hop-count headers.
* **Reachability.** A listener that accepts TCP and closes without answering HTTP is *not*
  reachable.
* **Rolling restart.** A clone loop against the load balancer during `rollout restart` sees no
  failures — the `preStop` delay is what this proves.
* **Real transport.** A real `git push` and `git clone` through a forwarding node, over HTTP and
  over SSH, produce correct results and leave exactly one node's pool warm. The HTTP push is over
  1 MiB so `Expect: 100-continue` and chunked framing are exercised; the SSH clone is a
  multi-command session on one connection.
* **Fenced handle.** A node whose repo is fenced by a peer that ranks above it returns 503 rather
  than reopening; a node fenced by a stray admin process reopens, because it is still `Local`.
* **Shutdown.** SIGTERM during an in-flight clone lets the clone finish and closes the pool.

## Deployment changes

Removed: the Ingress and its `upstream-hash-by` annotations, and the `kloudlite-http` Service.

Added: a `LoadBalancer` Service publishing 80 and 2222 across all pods; `KLOUDLITE_PEER_SVC`,
`KLOUDLITE_REPLICAS`, `KLOUDLITE_SELF` (the pod name, from the downward API) and
`KLOUDLITE_PEER_SECRET` (from a Secret) on the StatefulSet with the peer container ports,
published by no Service; a `preStop` sleep (15 s) so a terminating pod leaves the load balancer's
endpoints before it stops answering; and a NetworkPolicy allowing 8081 and 8082 only from pods
labelled `app: kloudlite`, kept for a cluster that enforces one.

The server refuses to start with `KLOUDLITE_PEER_SVC` set but any of `KLOUDLITE_REPLICAS`,
`KLOUDLITE_SELF` or `KLOUDLITE_PEER_SECRET` missing. A default for any of them would be a
phantom peer, a wrongly sized fleet, or an open port, and all fail silently.

`KLOUDLITE_REPLICAS` must match `spec.replicas`: it *is* the peer set. Scaling means editing both
and rolling.

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
