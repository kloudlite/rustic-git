# A read API in front of the git nodes

Cloning is not browsing. A clone wants every object and gets one answer; browsing wants one
directory, one file, one diff, and asks a thousand times. Serving the second from the machines
that serve the first means a crawler on a big repo competes with a `git push` for the same
disk, the same odb, and the same CPU.

This design puts the read API in its own process, in front of the git nodes, with a shared cache
between them. The git nodes gain browse handlers on a port that was already private, and lose
nothing else.

Extends the ownership design (the map, the leader, the leases, the forwarding are untouched) and
the object-serving design (clones still leave via signed URLs; this path never carries pack bytes).

## What it serves

Branches and tags, a directory listing, a file's contents, commit history, and a commit's diff.
JSON only — no HTML, no assets, no bundled frontend.

## The one decision everything follows from

A URL naming a branch is a moving target: `main` means something new after every push, so no
answer about it can be kept. A URL naming a commit id is not: `abc123` is a fingerprint of
content and can never mean anything else.

So branch names appear in exactly one endpoint, the ref list. It resolves `main → abc123`.
Every other endpoint takes the id.

```
GET /api/{o}/{n}/refs                 → [{name, oid, kind: branch|tag, peeled}]   MUTABLE
GET /api/{o}/{n}/tree/{oid}/{path}    → [{name, mode, kind, oid, size}]
GET /api/{o}/{n}/blob/{oid}/{path}    → bytes + {truncated}
GET /api/{o}/{n}/log/{oid}?path=&n=   → commits, cursor-paginated by oid
GET /api/{o}/{n}/commit/{oid}         → message, parents, diff
```

`{oid}` is a commit or tree id. Exactly one endpoint can go stale, and it holds for five seconds.
This is the entire caching design; everything below is bookkeeping.

## Shape

```
             ┌──────────────┐  hit: served here, no git node involved
  client ───►│  Cloudflare  │───►┌───────────────┐───► Redis
             │  WAF, DDoS   │    │ api Deployment│
             └──────────────┘    └───────┬───────┘
                                         │ miss, peer secret
                                         ▼
                                 ┌───────────────┐  route middleware forwards to the owner
                                 │  git Service  │───► owner node ──► odb ──► local packs
                                 └───────────────┘
```

Same binary, new subcommand: `rustic-git api-serve`. A stateless Deployment — no SlateDB, no local
disk, no ownership awareness, scaled by replica count alone. It needs object-store credentials
(for token lookups), the peer secret, and Redis.

The git nodes get the browse handlers on the **peer router only** (8081), which is already secret-
guarded and never publicly reachable. The public git router is unchanged.

The api server does not know who owns a repo and does not need to. It calls the git Service; the
existing `route` middleware forwards to the owner exactly as it does for git traffic.

## Why any api pod can answer

A cached answer is keyed by an immutable id, so it cannot be out of date, so the pod that happens
to receive the request can return it without involving the machine that owns the repo. That skips
the ~1.7s a cold repo costs to open and download. Only misses take the long path.

This is why ids and not branch names: under branch-named URLs only the owner could know whether a
cached copy was current, and the cache would buy nothing.

## Visibility

`meta/public` in the repo database, default private, set by
`admin set-visibility <owner>/<name> public|private`.

Public means anonymous read **and** anonymous clone. Push and admin always require the owner's
token. `authorize()` takes the flag as a third argument; both its callers (HTTP and SSH) pass it.

The api server authorizes without touching a git node: token → owner is a plain object-store read,
and the public flag comes from Redis (30s), fetched alongside the data on a miss.

## Cache

Azure Managed Redis, Balanced B0 to start, private endpoint, TLS, key from a Secret,
`maxmemory-policy volatile-lru`.

The policy is load-bearing, not a tuning preference. Data keys all carry a TTL; generation keys
carry none. Under `volatile-lru` only keys with an expiry are eviction candidates, so pressure
evicts cached answers and never the counters that decide which answers are still reachable. Under
`allkeys-lru` an evicted counter reads back as generation 1 — the pre-purge value — and every
stale entry it was meant to orphan becomes visible again.

Every key carries a per-repo generation: `v1:{gen}:{owner}/{name}:...`

| Key | TTL | Why |
|---|---|---|
| `tree:{oid}:{path}`, `blob:{oid}:{path}`, `log:{oid}:{cursor}`, `commit:{oid}` | 7d | immutable; TTL is an eviction hint, not correctness |
| `refs` | 5s | the only mutable answer |
| `meta` | 30s | the public flag, so a hit needs no git node |

Blobs are capped at 5 MB on the wire with a `truncated` flag, and are not cached above 1 MB.

**Redis unreachable fails open**: log and fall through to the git nodes. A cache outage costs
latency, never correctness and never availability.

## Invalidation

- **A ref moved** (receive-pack completion, admin ref writes): the owner node deletes that repo's
  `refs` key. Best-effort — it never fails the push, because the 5s TTL heals a missed delete.
- **Everything else** (visibility flipped, repo deleted, manual purge): `INCR gen:{owner}/{name}`.
  Every old key becomes unreachable at once and ages out under LRU. No `SCAN`, no key enumeration.
  `admin purge-cache <owner>/<name>` is exactly this INCR. Counters are never expired, so they
  accumulate one small integer per repo ever purged — the price of the guarantee below.

The generation counter is what makes public→private safe: the instant the flag flips, no cached
response for that repo can be served to anyone.

## Edge

Cloudflare handles rate limiting and DDoS. It caches nothing here — `/api/*` is bypassed, which is
also its default, since Cloudflare does not cache JSON without explicit rules.

- `/api/*` anonymous: 300 requests per 5 minutes per IP. With `Authorization` present: 3000 per 5
  minutes. Cloudflare's counters are per-POP, so thresholds below ~200/min enforce loosely; the
  windows are sized accordingly.
- Git HTTP paths: a separate, looser rule. Few requests, heavy each.

Two limits to accept:

- **SSH cannot traverse Cloudflare.** Port 2222 stays on the LoadBalancer, unrated and unshielded.
- **The origin must be locked to Cloudflare's ranges** (`loadBalancerSourceRanges`), or the WAF is
  one `curl` away from irrelevant.

Responses still carry `Cache-Control`: `public, max-age=31536000, immutable` for id-addressed
answers on public repos, `private, no-store` for anything private — so no proxy or browser
downstream holds a private repo, whatever the edge is configured to do.

## Errors

| Case | Response |
|---|---|
| unknown repo, or no permission | 404 — a private repo must not be distinguishable from a missing one |
| unknown oid or path | 404 |
| owner unreachable, repo unclaimable | 503, passed through from the git node |
| Redis down | invisible: served from the git nodes |
| blob over 5 MB | 200, truncated at the cap, `truncated: true` |

## Testing

Unit: path → oid resolution and tree traversal; the authz matrix (anonymous / wrong owner / owner ×
public / private × read / push); cache key construction including generation; the blob cap;
`Cache-Control` selection.

Integration: create repo → push → browse tree, blob, log, commit → `set-visibility public` →
anonymous browse and anonymous clone succeed → push still 401s → flip back to private → a
previously cached response is no longer served. Plus: kill Redis mid-suite and confirm every read
still answers.

## Deliberately not in scope

Search, blame, rendered markdown, an HTML frontend, and write operations through the API. Each is
a separate spec, and none of them change the shape above.
