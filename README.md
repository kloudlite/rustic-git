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
- **A dead node's repos are unavailable for up to the 10s lease TTL**, then claimed by whoever is
  next asked for them.
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

Then `kubectl apply -f deploy/rustic-git.yaml`.

Packs are unaffected: they are content-addressed and immutable, so every node reads them straight
from `objects/{owner}/{name}/`. Credentials live as plain object keys (`auth/...`), readable by
every node.

Opening a database is not free — measured at ~1.7s against a bucket in another region, ~0.8ms of
which is SlateDB itself; the rest is sequential object-store round trips. Put the nodes in the
bucket's region. `RUSTIC_GIT_WARM_TTL_SECS` (default 300) and `RUSTIC_GIT_MAX_WARM` (default 64)
keep recently used repos open so only the first request per repo pays it.

`admin` commands open the repos they touch, so stop the node serving those repos first (a concurrent
admin process fences the server).

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
rustic-git admin set-visibility <owner>/<name> public|private
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
- `RUSTIC_GIT_API_ADDR` — HTTP listen address (default `0.0.0.0:8090`).
- `RUSTIC_GIT_REDIS_URL` — optional. Without it, the api process still answers every request,
  just always by asking a git node instead of serving from cache.

## Cloning

```
git clone http://x:<token>@host:8080/owner/name.git
git clone ssh://git@host:2222/owner/name.git
```

HTTP basic auth accepts any username (e.g. `x`); only the password (the token) is checked.

## Browsing

`rustic-git api-serve` runs a separate, stateless read API in front of the git fleet
(`/api/{owner}/{name}/...`), backed by an optional Redis cache. Branch names appear in exactly
one endpoint, `/refs`, which resolves a name like `main` to the commit id it currently points at.
Every other endpoint — tree, blob, log, commit — takes that id, never a branch name.

That is what makes the cache work: an id is a fingerprint of content and can never mean something
else, so a cached answer keyed on it is never stale and any api pod can serve it without asking the
node that owns the repo. Only `/refs` is a moving target and is cached for 5 seconds instead of
being kept indefinitely.

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
