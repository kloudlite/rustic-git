# rustic-git

A git server that stores pack files in S3 (via `object_store`) and refs/tokens/keys
in SlateDB, speaking both the git HTTP smart protocol and SSH.

## Forks

A fork gets its own copy of the source's objects, made with the object store's server-side copy
so the bytes never pass through the server. Repos never share storage.

That costs duplicated bytes, which is the right trade here: forks are rare, object storage is
cheap, and sharing a pile means garbage collection has to see every repo using it — which
constrains where repos may live, requires cross-node coordination to collect, and makes
cross-repo object exposure possible at all. Owning objects outright makes collection a local,
per-repo operation and removes that exposure structurally.

## Running more than one node

One SlateDB database per repo, at `repo/{owner}/{name}`, and the repo is the unit of ownership. A
plain round-robin LoadBalancer sits in front and nothing there needs to understand git. Who owns
which repo is not derived — it is written down. `rustic-git-0` keeps a map of `repo → (node,
expires)` in its own SlateDB database at `cluster/ownership`, and is its only writer; every other
node opens it read-only and follows.

**Leadership is a name, not a decision.** Every node derives the leader from its own identity:
strip the ordinal off `RUSTIC_GIT_SELF`, append `-0`. A StatefulSet guarantees at most one pod per
ordinal, so two leaders cannot exist and there is no election to get wrong. There is deliberately
**no failover to ordinal one**: a leader that is unreachable blocks new claims; it is not replaced.

Routing a request is one local map read. If the map names this node, it serves; if it names another,
it forwards over the peer ports (8081 HTTP, 8082 stream); if nobody owns the repo, it asks the
leader for it — one round trip, and only when a repo is cold. A claim that cannot be granted (the
leader is unreachable) returns 503 rather than serving anyway, because serving on a failed claim is
exactly the two-writer bug this removes. A follower's copy of the map may be stale (it polls the
manifest every 200ms); that costs one extra hop and can never produce a second owner, because only
the leader grants.

**A node holds a repo's lease exactly as long as it holds that repo's database open.** A claim
precedes the open, a renewal every 3s (batched, one message per node) continues while the handle is
held, and eviction begins with a release. Releasing does not delete the entry — it shortens it to a
500ms drain, during which this node is still the owner and still serving; only then is the database
closed. Deleting instead, or closing before the drain, lets another node open a database this one is
still holding and fence it, which is a failure this system has already produced on a real cluster.
The other direction holds too: a node whose renewal is declined has lost the lease and closes that
database at once rather than waiting to be fenced.

The peer ports carry a shared secret (`RUSTIC_GIT_PEER_SECRET`, from Secret `rustic-git-peer`)
because this cluster runs with `networkPolicy: none`: anything on the pod network can otherwise reach
them. Scaling is `spec.replicas` alone — there is no peer list to keep in step.

Read-only replica nodes were removed. A follower can only serve refs as stale as its last manifest
poll (~1s), which breaks read-your-own-writes — push, then fetch from another node and the commit is
missing. Since repos are already the unit of ownership, sending a repo's whole traffic to one node
costs nothing and removes the staleness window entirely. Fanout across repos, not within one.

**Limits worth knowing:**

- **While `rustic-git-0` is restarting, no repo can be claimed.** ~20s, measured on this cluster.
  Repos already open keep serving throughout — their holders have the databases and renewals are
  advisory — and the map survives the restart, so nothing is rebuilt. Cold repos get a 503.
- **A node partitioned from the leader** keeps serving what it holds and cannot claim anything new.
  It does not become leader.
- **A dead node's repos move within about a second**, not in the 10s lease TTL. A node that fails to
  connect to the owner waits 350ms, re-reads the map, and — if it still names the same unreachable
  node — asks the leader to force the repo over. Only GET requests, only connect failures, and only
  after that second failure: one dropped connect never moves a repo. The leader refuses to
  force-grant an entry written in the last second, so two nodes recovering from the same dead owner
  cannot ping-pong the repo; the loser is told the winner and forwards there. If the old owner was
  in fact alive and merely unreachable, the grant fences it and an in-flight push there fails and is
  retried — the intended trade against ten seconds of 502s. A repo whose lease simply lapses is
  still claimed by whoever is next asked for it.
- **A stale grant is not a correctness problem.** SlateDB's writer epoch fences the second opener,
  whose pool reports it and re-routes. The map buys accuracy; fencing is what buys safety.
- **On SIGTERM the two HTTP listeners drain; SSH sessions do not.** An SSH session in flight on a
  terminating pod is cut when the drain ends; the preStop delay is what makes that rare (see the
  manifest comment for the timing arithmetic). The pool releases every lease and drains before it
  closes, so peers take the repos over without fencing anything.
- **Liveness is `/healthz`, which reflects the object store.** During an object-store outage longer
  than ~90s every pod is restarted, which achieves nothing but is harmless — the pods come back into
  the same outage.
- **Single node** (`RUSTIC_GIT_PEER_SVC` unset) runs with no ownership map at all: one node owns
  everything by construction, so there is nothing to claim, renew or prune.

### Deploying

The peer ports need a shared secret because this cluster enforces no NetworkPolicy
(`az aks show -n kolomi-cluster -g kolomi-rg --query networkProfile.networkPolicy -o tsv` → `none`):

```
kubectl -n rustic-git create secret generic rustic-git-peer \
  --from-literal=secret="$(openssl rand -hex 32)"
```

The read API needs a Redis URL in a secret. Without it the api pods still run — every request
just goes to a git node instead of the cache:

```
kubectl -n rustic-git create secret generic rustic-git-redis \
  --from-literal=url="rediss://:<key>@<host>:10000"
```

Both tiers need that secret, not just the api pods. Invalidation runs on the **git nodes** — a push
drops the repo's `refs` entry, a visibility flip or a delete bumps its generation — so a fleet
without the Redis URL caches answers that nothing can ever purge, and `set-visibility private`
reports success while orphaning nothing. A disabled cache returning success is correct for reads
and silent in exactly this case, which is why the URL belongs on both workloads.

**Redis must run `maxmemory-policy volatile-lru`, and this is correctness, not tuning.** Cached
answers carry a TTL; the per-repo generation counters that decide which answers are still reachable
carry none. Under `volatile-lru` only keys with an expiry are eviction candidates, so pressure
evicts answers and never the counters. Under `allkeys-lru` an evicted counter reads back as
generation 1 — the value from before the last purge — and every stale answer it was meant to orphan
becomes visible again, including for a repo that was just made private.

Then `kubectl apply -f deploy/rustic-git.yaml`.

The manifest pins both workloads to an image digest tag rather than `:latest`, so applying it is an
explicit decision about which build runs rather than whatever was pushed last.

The api tier ships as a `ClusterIP` Service. It is meant to sit behind Cloudflare, which supplies
the rate limiting and DDoS protection this codebase deliberately does not implement; exposing it as
a `LoadBalancer` before that is in place puts an unmetered read API on the internet. When you do
switch it, set `loadBalancerSourceRanges` to Cloudflare's ranges in the same change — the manifest
ships a deliberately invalid placeholder so a premature apply is rejected by the API server rather
than silently accepted. Do not "fix" that rejection by deleting the field: absent means open to
everyone, with no warning.

Two limits worth knowing before you rely on the edge: SSH on 2222 cannot traverse Cloudflare's HTTP
proxy, so git-over-SSH is neither rate limited nor shielded by it; and the origin must be locked to
Cloudflare's ranges or the whole edge is one `curl` away from irrelevant.

Packs are unaffected: they are content-addressed and immutable, so every node reads them straight
from `objects/{owner}/{name}/`. Credentials live as plain object keys (`auth/...`), readable by
every node.

Opening a database is not free — measured at ~1.7s against a bucket in another region, ~0.8ms of
which is SlateDB itself; the rest is sequential object-store round trips. Put the nodes in the
bucket's region. `RUSTIC_GIT_WARM_TTL_SECS` (default 300) and `RUSTIC_GIT_MAX_WARM` (default 64)
keep recently used repos open so only the first request per repo pays it.

Most `admin` commands open the repos they touch, so stop the node serving those repos first (a
concurrent admin process fences the server). `set-visibility` is the exception: with either
`RUSTIC_GIT_UPSTREAM` or `RUSTIC_GIT_PEER_SECRET` set it posts to the peer Service instead, and the
routing middleware delivers the write to the node that owns the repo — nothing to stop, and no
second writer. With NEITHER set it writes directly and says so on stderr: this process cannot see
whether a node is serving the repo, so the direct write is an assumption it is announcing, not a
guarantee it can make. Export both on any box that administers a live fleet.

`GET /healthz` returns 200 and the warm-database count. Tokens are stored hashed; re-issue any token
minted before this (`admin add-token`).

## Usage

```
rustic-git serve
rustic-git admin create-repo <owner>/<name>
rustic-git admin fork <src-owner>/<src-name> <owner>/<name>   # copies objects and refs
rustic-git admin delete-repo <owner>/<name>
rustic-git admin repack <owner>/<name>                        # consolidate the fork network into one pack
rustic-git admin add-token <owner>        # prints a new access token
rustic-git admin add-key <owner> <pubkey-file>
rustic-git admin set-visibility <owner>/<name> public|private   # routed to the repo's owner when RUSTIC_GIT_PEER_SECRET is set
rustic-git admin purge-cache <owner>/<name>
rustic-git api-serve                                          # read API; needs RUSTIC_GIT_UPSTREAM
```

## Environment variables

- `RUSTIC_GIT_S3_URL` — **required** for all commands: object store URL, e.g. `s3://bucket`
  (reads `AWS_*` env vars). `mem://` is an in-memory store for testing only — nothing is
  persisted and everything is lost on exit.
- `AWS_PROFILE` — if `AWS_ACCESS_KEY_ID` is unset, static keys and `region`/`endpoint_url` are read
  from `~/.aws/credentials` and `~/.aws/config` for this profile (default `default`).
  SSO / assume-role profiles are not supported.
- `AWS_ENDPOINT`, `AWS_REGION` — for S3-compatible stores. Example for DigitalOcean Spaces:
  `AWS_PROFILE=do AWS_ENDPOINT=https://sgp1.digitaloceanspaces.com AWS_REGION=sgp1 RUSTIC_GIT_S3_URL=s3://rustic-git`

The rest apply to `serve`:

- `RUSTIC_GIT_S3_TIMEOUT_SECS` — per-request S3 timeout (default 900). Repack uploads a whole
  network in one PUT, so a distant bucket needs more than object_store's 180s default.
- `RUSTIC_GIT_FLUSH_INTERVAL_MS` — how often the ref store flushes its write-ahead log
  (default 100). A ref update waits for the next flush, so this sets push latency when pushes
  arrive one at a time; lowering it costs more object-store writes. See "Write throughput".
- `RUSTIC_GIT_MAX_BODY` — max request body in bytes (default 2 GiB). Enforced before authentication.
- `RUSTIC_GIT_CACHE_DIR` — local pack/object cache directory (default `./cache`).
- `RUSTIC_GIT_WARM_TTL_SECS` — how long an unused repo database stays open (default 300).
- `RUSTIC_GIT_MAX_WARM` — ceiling on simultaneously open repo databases (default 64).
- `RUSTIC_GIT_HTTP_ADDR` — HTTP listen address (default `0.0.0.0:8080`).
- `RUSTIC_GIT_SSH_ADDR` — SSH listen address (default `0.0.0.0:2222`).
- `RUSTIC_GIT_HOST_KEY` — path to an OpenSSH host key; generated if missing (default `./host_key`).
- `RUSTIC_GIT_PEER_SVC` — headless Service FQDN the peer hostnames hang off (e.g.
  `rustic-git.rustic-git.svc.cluster.local`). Unset means single-node: no ownership routing.
- `RUSTIC_GIT_SELF` — this pod's stable name (`rustic-git-2`). Its ordinal replaced by 0 is the
  leader's name, and the map records ownership under it. Required when `RUSTIC_GIT_PEER_SVC` is set.
- `RUSTIC_GIT_PEER_SECRET` — shared secret for the peer ports. Required when `RUSTIC_GIT_PEER_SVC`
  is set.
- `RUSTIC_GIT_PEER_ADDR` — peer HTTP listen address (default `0.0.0.0:8081`). The peer stream port
  is derived as peer port + 1 (8082 by default), not separately configurable.

The rest apply to `api-serve`:

- `RUSTIC_GIT_UPSTREAM` — base URL of the git fleet's **peer** Service (default
  `http://rustic-git:8081`), not the public one: browse routes are only mounted on the peer
  listener.
- `RUSTIC_GIT_PEER_SECRET` — **required**: the same shared secret the git nodes run with. The api
  process talks to them over the peer listener and refuses to start without it.
- `RUSTIC_GIT_API_ADDR` — HTTP listen address (default `0.0.0.0:8090`).
- `RUSTIC_GIT_REDIS_URL` — optional. Without it, the api process still answers every request,
  just always by asking a git node instead of serving from cache.

## Cloning

```
git clone http://x:<token>@host:8080/owner/name.git
git clone ssh://git@host:2222/owner/name.git
```

HTTP basic auth accepts any username (e.g. `x`); only the password (the token) is checked.

The server speaks git protocol v2 only, no v0/v1 fallback. git 2.26+ defaults to v2; older
clients need `git -c protocol.version=2 <command>`.

## Browsing

`rustic-git api-serve` runs a separate, stateless read API in front of the git fleet
(`/api/{owner}/{name}/...`), backed by an optional Redis cache. Branch names appear in exactly
one endpoint, `/refs`, which resolves a name like `main` to the commit id it currently points at.
Every other endpoint — tree, blob, log, commit — takes that id, never a branch name.

That is what makes the cache work: an id is a fingerprint of content and can never mean something
else, so a cached answer keyed on it is never stale and any api pod can serve it without asking the
node that owns the repo. Only `/refs` is a moving target and is cached for 5 seconds instead of
being kept indefinitely.

### The whole flow

```mermaid
sequenceDiagram
    autonumber
    participant C as client
    participant CF as Cloudflare<br/>(rate limit, DDoS)
    participant A as api pod<br/>(rustic-git api-serve)
    participant R as Redis
    participant S as object store<br/>(tokens, packs)
    participant P as peer Service :8081
    participant O as owner node<br/>(holds the repo DB)

    C->>CF: GET /api/alice/web/tree/{oid}/src
    CF->>A: forwarded (bypassing its own cache)

    Note over A: one parsed path drives<br/>authz, cache key and upstream URL
    A->>S: token -> owner (plain object read)
    S-->>A: alice
    A->>R: GET meta (is the repo public?)
    R-->>A: 1 / miss

    alt cached and caller authorized
        A->>R: GET v1:{gen}:alice/web:tree:{oid}:src
        R-->>A: body
        A-->>C: 200 (no git node involved)
    else miss
        A->>R: GET gen (captured before the call)
        A->>P: same request + peer secret + owner header
        Note over P,O: route middleware forwards<br/>to whoever owns the repo
        P->>O: /api/alice/web/tree/{oid}/src
        O->>O: open_repo -> gix odb over local packs
        O-->>A: JSON
        A->>R: SET at the captured generation<br/>(a purge mid-flight lands it out of reach)
        A-->>C: 200
    end

    Note over C,O: writes invalidate only what can go stale
    C->>O: git push (receive-pack)
    O->>R: DEL refs (best effort, 5s TTL heals a miss)
    C->>O: admin set-visibility private
    O->>R: INCR gen (must succeed, or the command fails)
```

Every URL except `/refs` names an object id, so the cache hit at the top needs no git node at all —
that is the entire point of the shape.

### Visibility

Repos are private by default: reads and clones need a token whose owner matches the repo's owner.
`admin set-visibility <owner>/<name> public` opens a repo to reads *and* clones by **everyone** —
anonymous callers and any authenticated caller alike, not just the owner. Presenting a token never
grants less than presenting none. Pushing and admin always require the owner's token, public or
not: public grants read, never identity.

The flip is the one admin write that changes live authorization, so it does not touch the database
directly: it goes to `POST /api/{owner}/{name}/visibility` on the peer listener, and the routing
middleware forwards it to the node that owns the repo. One writer, one view — a direct write from a
second process would leave the serving node authorizing from its own stale handle for seconds. That
endpoint is peer-only: an `/api/` request on the public listener is refused, never forwarded.

A private repo answers 404, never 403 — a stranger cannot tell it from a repo that does not exist.

Flipping a repo back to private bumps a per-repo generation counter, which makes every cached
answer for it unreachable at once. That call can fail (Redis may be down), and when it does the
command fails loudly rather than reporting a success it did not achieve: the repo is private in the
database but its cached answers are not yet orphaned, and `admin purge-cache <owner>/<name>` is the
retry. This is the one place the cache is *not* allowed to fail quietly — everywhere else a cache
outage only costs latency, here it would cost the guarantee itself.

### `api` is a reserved owner name

No repo may be owned by `api`, because `/api/{owner}/{name}/...` would otherwise be both that
repo's git route and another repo's browse route — and the routing middleware and the HTTP router
resolve that ambiguity differently, which is how one repo's request ends up routed by another
repo's ownership. Reserving the name removes the ambiguity instead of adjudicating it. A repo
created before this reservation keeps working over SSH and can be moved with `admin fork`; its
git-HTTP routes are gone.

## Pack index

A node needs to know which pack files a repo has before it can serve anything. That used to be a
listing of the object store on every request — a network round trip in front of every clone, fetch
and push. A node records each pack in the ref store as it uploads it, so the list comes from the
same database as the refs: no listing, and the pack set always matches the refs alongside it. Repos written before the index existed fall back to one listing, which is then
recorded.

Measured against a bucket in another region, the server-side advertisement went from ~136ms to
~51ms. End-to-end `git ls-remote` is unchanged at ~0.3s, being dominated by process startup and two
HTTP round trips; the win is in server-side work and in not spending an object-store request per
git request, which matters for rate limits and cost long before it shows up as latency.

## Write throughput

Measured with `cargo test --release --test throughput -- --ignored --nocapture` against
DigitalOcean Spaces in Singapore (~200ms RTT), one node. A push costs one ref-update
transaction, so this is the ref store's ceiling per node:

| concurrent ref updates | durable ops/sec |
|---|---|
| 1 | ~9 |
| 8 | ~70 |
| 32 | ~300 |
| 128 | ~1000 |
| 512 | ~2500 (plateau) |

Writes are durable before returning (`await_durable`), and object-store latency is largely
hidden: the same benchmark against an in-memory store gives nearly identical numbers, because
concurrent commits batch into one flush. What a single client feels is therefore not bandwidth
but the flush cadence — roughly 70-115ms per push, floored by the write-ahead log flush interval
and the round trip of one small object write. Lowering `RUSTIC_GIT_FLUSH_INTERVAL_MS` from 100 to
5 moves serial throughput from ~9 to ~14 ops/sec and no further; past that the object store's own
latency dominates.

The practical reading: the ref store is not the bottleneck for a git server. Pack indexing (CPU)
and pack upload (bandwidth) will saturate long before 2500 pushes/sec. Spread repos across nodes to
multiply this figure — each repo is an independent database.

## License

Server Side Public License v1 (SSPL-1.0). See [LICENSE](LICENSE).
