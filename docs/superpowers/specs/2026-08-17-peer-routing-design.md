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

## Ownership: rendezvous hash

Each node scores every peer for a repo and takes the highest:

    owner(repo) = argmax over peers of fnv1a(repo || peer)

Properties that matter here:

* **No coordination.** Every node computes the same answer from the same inputs. There is no
  lookup on the request path, no lease to renew, nothing to expire, and no state that can be
  stale or disagree between nodes.
* **Minimal movement.** Adding or removing a peer moves roughly 1/N of repos, not all of them.
  A plain `hash % N` would reshuffle nearly everything on every scaling event.
* **Failure is already handled.** A repo whose owner changes while its database is open is the
  case SlateDB fencing exists for: the new owner takes the writer epoch, the old holder's next
  call fails, and the pool drops the dead handle and reopens. Noisy for one request, not lossy.

Membership is static configuration rather than discovery: `RUSTIC_GIT_PEERS` lists every peer's
base URL, `RUSTIC_GIT_SELF` says which one this node is. A StatefulSet gives stable DNS names
(`rustic-git-0.rustic-git`), so the list is known at deploy time. Discovery through the
Kubernetes API would track membership faster, at the cost of RBAC, a Kubernetes client, and
binding the server to running inside Kubernetes. Not worth it while the peer set changes only
when someone edits `replicas`.

Scaling is therefore a config change plus a rolling restart, and repos move. That is the accepted
cost of having no ownership state.

## Data flow

    HTTP client ─▶ LB ─▶ node B ─┬─ owner? ──────▶ serve locally
                                 └─ not owner ───▶ proxy to node A ─▶ stream back

    SSH client ─▶ LB ─▶ node B: terminate SSH, authenticate the key,
                                parse exec 'git-upload-pack /o/r.git'
                                ├─ owner? ──────▶ serve locally
                                └─ not owner ───▶ POST /o/r/git-upload-pack to node A
                                                  channel stdin ⇄ request body
                                                  response ⇄ channel stdout

Both edges converge on one internal protocol: the git smart-HTTP call the node already serves.
SSH becomes a translation layer at the edge rather than a second forwarding path, and there is no
third protocol to write, version, or secure.

Bodies are streamed in both directions. A push is a single large request body and a clone is a
long response; neither may be buffered in memory, and a client that disappears must cancel the
work on the owner as it does today.

## Peer authentication

A forwarded request must tell the owner who authenticated, because the credential was checked at
the edge and is not re-presented.

    X-Rustic-Git-Peer:  <shared secret>      # this request came from a node, not a client
    X-Rustic-Git-Owner: <authenticated owner> # who the edge authenticated

The owner header is honoured **only** when the secret matches; otherwise the request is
authenticated normally, as any client request is. The secret comes from a Kubernetes Secret, not
from the image or the manifest.

This is the security boundary of the design. Port 8080 serves both clients and peers, so without
the secret check any client could assert any identity by setting a header. Tests must cover a
forged owner header with no secret, and with a wrong secret.

The peer header doubles as a loop guard: a request that arrives carrying it is served locally
whatever the hash says. Two nodes that transiently disagree — mid-roll, mid-scale — then produce
one wasted hop rather than an infinite chain.

## Failure handling

* **Peer unreachable.** Return 503 with a plain message. git retries, and Kubernetes restarts the
  pod. No health tracking, no failover to a second-choice node: failing over would put a repo on
  a node the rest of the fleet does not consider the owner, which is the fencing scenario this
  design exists to avoid.
* **Fenced handle.** Already handled in `Pool::get`: a closed database is dropped and reopened.
* **Client disconnects.** Unchanged — work is cancelled when the client goes away. The forwarding
  node must propagate cancellation to the owner rather than leaving an orphaned request.

## Components

| Unit | Responsibility | Depends on |
|---|---|---|
| `peers` | Parse the peer list; `owner(repo) -> &Peer`; `is_self()` | nothing |
| `proxy` | Forward one HTTP request to a peer, streaming both ways | HTTP client, `peers` |
| `http` | Decide owner-or-forward before handling a request | `peers`, `proxy` |
| `ssh` | After exec parsing, serve locally or translate into a peer HTTP call | `peers`, `proxy` |

`peers` is pure computation and testable without any I/O — the property that matters most, since
every node agreeing is the correctness condition.

## Testing

* **Rendezvous hash.** Determinism; every node computes the same owner for the same repo; removing
  a peer moves roughly 1/N of repos and leaves the rest untouched.
* **Forwarding.** Two servers in one process over an in-memory store. A request to the non-owner
  is served correctly, and the *owner* is the node that opened the database — asserted through the
  pool's warm count, so the test fails if both nodes open it.
* **Peer auth.** A forged `X-Rustic-Git-Owner` without the secret, and with a wrong secret, is
  rejected. A request carrying the correct secret is served locally even when the hash disagrees.
* **SSH translation.** An SSH clone of a repo owned by another node returns the same bytes as a
  local one.

## Deployment changes

Removed: the Ingress and its `upstream-hash-by` annotations, and the `rustic-git-http` Service.
Added: a `LoadBalancer` Service publishing 80 and 2222 across all pods, a Secret holding the peer
secret, and `RUSTIC_GIT_PEERS` / `RUSTIC_GIT_SELF` on the StatefulSet.

## Not in scope

**Serving object traffic from any node.** Reads and writes of pack files could in principle run
anywhere — packs are immutable and every node reads them straight from blob storage — but today
every git route touches the refs database: `upload-pack` computes reachable tips
(`protocol/upload.rs`) and the pack index lives in the same database. Freeing clone traffic from
the owner needs a read-only ref view, which is a separate design with its own staleness
trade-offs. Clone bandwidth is the reason to want it, so it is likely the next piece of work.

**Ownership that survives scaling.** A lease per repo would pin ownership across membership
changes, at the cost of an object-store read on the routing path and the claim/renew machinery
deleted in `1a558f9`. Revisit only if scaling events prove disruptive in practice.
