# Objects leave the owner: signed URLs instead of a byte pipe

A repo's database may be open on exactly one node, and today that node also carries every byte
the repo ever serves. Ownership is about *refs* — they are mutable, and single-writer is what keeps
them consistent. Objects are not: a packfile is immutable, content-addressed, and sitting in blob
storage that every node and every client can already reach. Routing them through the owner buys
nothing and costs the owner all of its bandwidth.

This design stops moving object bytes through the fleet. Clones are answered with signed URLs and
the client fetches packs from blob storage directly.

Supersedes nothing. Extends the ownership design: the map, the leader, the leases and the
forwarding all stay exactly as they are.

## What actually needs the owner

Measured and read from the code, not assumed:

| Request | Objects touched | Needs the owner? |
|---|---|---|
| `ls-refs` / `info/refs` | none | refs only |
| full clone (no `have` lines) | all of them | pack list only |
| incremental fetch (`have` lines) | graph traversal | yes — see below |
| push (`git-receive-pack`) | writes refs | yes |

`upload_pack` reads through `repo.odb()` — a gix-odb over locally mmap'd `.idx`/`.pack` files — and
`reachable_set` walks the graph object by object. That is random access with delta chains, roughly
a microsecond per read locally. The same access pattern over HTTP range requests is about ten
milliseconds per read: four orders of magnitude worse, and no amount of caching hides it. **Any
design where a node resolves objects itself needs those packs on local disk.** That is why the pack
cache exists, and it is why the answer is not to spread the cache further but to stop resolving
objects on the server at all for the case that matters.

## The case that matters

A full clone — a fetch with no `have` lines — wants every object reachable from the advertised
refs. Every object lives in a pack under `objects/{owner}/{name}/pack`; there are no loose objects
in this system. So the answer to "which bytes does this client need" is exactly "all of this repo's
packs", and that answer requires no traversal, no odb, and no local copy of anything.

Git protocol v2 can say precisely that. The `packfile-uris` capability lets the server reply with a
list of URIs instead of the bytes; the client fetches them itself and applies them. GitHub and
Azure DevOps both serve clones this way.

```
client: fetch, packfile-uris=https
server: packfile-uris
        <sha1> https://<account>.blob.core.windows.net/<c>/objects/alice/web/pack/pack-abc.pack?<sas>
        <sha1> https://.../pack-def.pack?<sas>
        packfile
        <empty>
```

The node answering that request sends a few hundred bytes. The pack bytes never enter the cluster.

## Shape

```
             ┌──────────────┐   snapshot: refs + pack list (one read, one snapshot)
   client ──►│ any node     │──────────────────────────────────►┌─────────────┐
             │ (auth here)  │◄──────────────────────────────────│ owner       │
             └──────┬───────┘                                   └─────────────┘
                    │ packfile-uris (signed, short-lived)
                    ▼
   client ─────────────────────────────────────────────────────► blob storage
                              pack bytes, direct
```

### The snapshot call

One new endpoint on the **peer** router, secret-guarded like the rest: given a repo, return its
refs and its pack list — names and sizes — read from the owner's database in a single read.

Both halves must come from one read. They already live in one database precisely so they stay
consistent, and this design depends on that: every ref advertised to the client must point into a
pack the client was handed a URL for. A pack written after the snapshot is simply not advertised
yet, which is what a client sees anyway when it fetches a moment earlier.

There is no replica and no follower read here. A node asks the owner and waits. That is the
constraint this design was given — a non-owner may not act on a lagging copy — and it is also
what makes the result trustworthy: the refs are the owner's, as of now.

**This is not a latency win, and should not be sold as one.** Asking the owner for a snapshot costs
the same round trip as forwarding the request to the owner. What changes is what crosses the wire
afterwards: a few hundred bytes of URLs instead of the whole repository.

### When URIs are used

Only for a fetch with no `have` lines — a clone. With `have` lines the client already has most of
the history, and handing it every pack would make it download the repository again to save a few
objects. Those requests keep the current path: forwarded to the owner, which has the packs warm and
resolves them locally.

A client that does not offer `packfile-uris` also keeps the current path. The local pack cache
therefore does not go away; it stops being the mechanism this design rests on and becomes the
fallback for old clients and incremental fetches.

## Signed URLs

Read-only SAS URLs, one per pack object, `SAS_TTL` = 5 minutes, issued only after the request has
been authenticated — authentication already runs before the handler that would issue them.

A signed URL grants access to that one blob for its lifetime without passing through us again. That
is the trade every implementation of this makes, and one earlier decision is what makes it safe
here. From `store.rs`:

> Every repo owns its objects outright. Forks copy rather than share.

Because packs are namespaced per repo and never shared between them, a URL for `alice/web`'s pack
cannot expose `bob/web`'s objects. Had forks shared an object pile, signed URLs would have been a
cross-repo read primitive and this design would be unavailable.

## Repack must not delete a pack a client is still fetching

`Gc::repack` deletes superseded packs from blob storage. Today that is safe: only the owner reads
them, and it holds the repack lease. Once clients hold URLs, a delete can land between issuing a
URL and the client using it, and the clone fails with a 404 partway through.

**Superseded packs are not deleted at repack time.** Repack records them as superseded with a
timestamp; a later pass deletes those older than `SUPERSEDED_GRACE` = 24 hours. The window has to
cover a slow client working through a large repository, not merely the SAS lifetime — the client
may open the last pack long after the first. Repack is a manual operation here, not a timer, so
this costs one extra copy of a repacked repo for a day and nothing in the steady state.

## Failure modes

* **The owner is unreachable when the snapshot is asked for.** Same as any other request that needs
  the owner today: it forwards or 503s, and the forward-recovery path applies unchanged.
* **A pack 404s despite the grace period.** The client's clone fails and it retries; the retry gets
  a fresh snapshot naming the current packs. Bounded, self-correcting, and visible in logs.
* **A SAS URL expires mid-clone.** The client fetches packs promptly after receiving them; a stalled
  client that resumes after five minutes fails and retries. If this proves common, raise `SAS_TTL`
  rather than adding refresh machinery.
* **Blob storage is unreachable from the client's network.** The clone fails in a way the current
  design would have survived, since today the bytes come from us. This is the real cost of the
  design and it is worth stating: we trade our bandwidth for a dependency on the client reaching
  Azure Blob directly.

## What this does not do

Incremental fetches and pushes still go to the owner, and still need the pack cache. This design
does not make a hot repository's *writes* scale, and it does not remove the owner from the read
path — it removes the owner from the read *payload*, which is where the bytes are.

## Numbers this rests on, and which are not yet measured

Measured in-cluster: a warm request is 13ms; a cold repo costs 297ms to claim and open.

Estimated, and worth measuring before committing to the incremental-fetch half: blob round-trip
latency (~10ms assumed) and single-stream throughput. The clone half does not depend on them — the
client pays those costs, not us.
