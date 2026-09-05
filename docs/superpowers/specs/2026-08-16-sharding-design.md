# Sharded multi-node kloudlite

Date: 2026-08-16 (follows the multi-node fencing test)

## Problem

SlateDB allows one writer per database. Two `kloudlite serve` processes on one bucket fence each
other: the loser fails every read and write (measured — see "Fencing" below). So the current design
caps at one node, and that node carries all clone, fetch and push traffic.

## Approach: shard the ref store, share the object store

Objects are already safe to share. Packs live at `objects/{network}/pack/`, names are content
hashes, and nothing mutates a pack in place — so any number of nodes can write packs concurrently.
S3 has no single-writer constraint. Only the ref/metadata store (SlateDB) does.

So: split SlateDB into `S` independent databases, one per shard, at `slatedb/shard-{i}`. Each is
its own single-writer domain. A node opens **only the shards it owns** as writers, so two nodes with
disjoint ownership never fence each other.

    shard(repo) = fnv1a("{owner}/{name}") % S

Sharding on the repo path (not the fork network) is deliberate: the shard is computable from the
URL alone, with no lookup, which is what lets any node — or an L7 load balancer — route a request
without consulting the database first.

### Ownership and routing

- `KLOUDLITE_SHARDS` — total shards, fixed for the deployment (default 1).
- `KLOUDLITE_OWNED_SHARDS` — this node's shards, e.g. `0,1` (default: all).
- `KLOUDLITE_SHARD_URLS` — `0=https://a.example,1=https://b.example`, used to redirect.

A request for a repo this node does not own gets `307` to the owning node. Git follows redirects on
`info/refs` and re-bases the subsequent POST on the redirected URL, so a plain `git clone` works
against any node in the fleet. An external load balancer can do the same routing with a hash rule
and skip the redirect hop; the redirect is what makes the fleet work without one.

SSH has no redirect mechanism: a node refuses repos it does not own and names the right host in the
error. Point SSH clients at the correct node, or front SSH with a hash-routing proxy.

### Auth

Tokens and SSH keys are global, so they live on shard 0 and other nodes read them through a
`DbReader` (read-only, no fencing). Consequence: a token minted on the shard-0 node becomes visible
to other nodes after their reader refreshes, not instantly. Acceptable for credentials that are
created rarely; documented rather than engineered around.

### Cross-shard operations

- `fork` reads the source repo's refs (possibly a foreign shard, via reader) and writes the new repo
  on its own shard. The write half stays atomic; the source is untouched.
- `repack` needs every repo in a fork network, which may span shards. It scans all shards (readers
  for foreign ones) and takes the network lock on the shard owning the network's root repo.
- Both are admin operations, run rarely, and neither is on the request path.

### Read replicas (optional, composes with the above)

`KLOUDLITE_SERVE_FOREIGN_READS=1` lets a node serve clone/fetch for shards it does not own, from
its `DbReader`, instead of redirecting. This scales reads beyond the owning node, at the cost of
refs lagging by the reader's refresh interval — a client can fetch and not see a commit pushed
seconds ago. Off by default: read-after-write is what git users expect.

## What this does not solve

- Resharding. `S` is fixed; changing it moves repos between shards and needs a migration. Consistent
  hashing would reduce the movement and is not built.
- A single repo is still served by a single node. This distributes load across repos, not within one.
- Two nodes claiming the same shard still fence. Ownership must be disjoint; the fenced node exits
  (status 3) so the mistake is loud.

## Compatibility

With `KLOUDLITE_SHARDS=1` (the default) the database path stays `slatedb`, exactly where existing
deployments have it. No migration for single-node installs.

## Fencing, measured

Two nodes, one bucket, before the fail-fast fix: the older node returned 500 for every request
including reads (`Closed error: detected newer DB client`), never recovered, and restarting it
fenced the other in turn. No data was lost. A fenced node now exits with status 3, and `/healthz`
reports database health for load balancer probes.

## Update: forks copy, they do not share

Fork networks are gone. Each repo owns `objects/{owner}/{name}/pack/` outright and a fork copies
the source's packs (server-side, ~12s for 46MB).

The reason is placement. Sharing a pile means garbage collection must see every repo using it, so
those repos have to be reachable together — either co-located on one shard, which lets a fork in
another org dictate where that org's data lives, or reachable by cross-node query, which makes GC
availability the product of every node's availability. Neither is acceptable multi-tenant.

Copying costs duplicated storage. Forks are rare and storage is cheap, and in exchange collection
becomes local to one repo, repos can live anywhere, and cross-repo object exposure stops being
possible rather than merely being checked for.
