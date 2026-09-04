# kloudlite-git — Git server with S3 objects + SlateDB refs

Date: 2026-08-16

## Goal
A single-binary Git server in Rust. Git objects live in S3; refs, pack indexes and auth live in embedded SlateDB (also backed by the same S3 bucket). Serves Smart HTTP and SSH.

## Non-goals (v1)
Web UI, PR/issues, hooks, LFS, delta compression on fetch, pack reuse, GC/repack, multi-instance coordination.

## Crates
`gix-odb`, `gix-pack`, `gix-object`, `gix-hash`, `gix-traverse` (object db, packs, walking), `slatedb` (re-exports `object_store` for S3), `axum`, `russh`, `tokio`, `flate2`. One crate: lib + thin bin.

## Revisions after API research (supersede sections below where they conflict)
1. **Packs only, no loose objects.** gix's `Find` trait is synchronous random-access; backing it with per-object S3 GETs is slow and needs a custom ODB. Instead every push is stored as a pack (`.pack` + `.idx`) in S3, and each instance keeps a **local disk cache** `cache/{owner}/{name}/objects/pack/` mirroring the S3 pack list. `gix_odb::at(cache_dir)` then handles all lookups, delta resolution and pack reuse. `Repo::open` = LIST S3 pack prefix, download missing files. `ponytail:` many small packs accumulate; add repack when clone time hurts. `ponytail:` LIST per request; cache the listing when it shows in latency.
2. **Ref updates use SlateDB serializable transactions** (`db.begin(SerializableSnapshot)`, read old → assert → put → commit) instead of a per-repo mutex. Works across instances.
3. **Pack `.idx` lives in S3 next to the `.pack`**, not in SlateDB (it's only read from the local cache).
4. **`pktline.rs` is hand-written** (~40 lines sync Read/Write) — smaller than adopting `gix-packetline`'s streaming reader.
5. **Protocol code is synchronous** (`fn(&mut dyn BufRead, &mut dyn Write)`) and runs in `spawn_blocking`; HTTP buffers request/response bodies, SSH bridges the channel with `tokio_util::io::SyncIoBridge`. gix is sync anyway.
6. **receive-pack speaks protocol v0** (git has no v2 receive-pack); **upload-pack speaks v2 only**.


## Storage

### SlateDB keys
| key | value |
|---|---|
| `repo/{owner}/{name}` | json: `{default_branch}` |
| `ref/{owner}/{name}/{refname}` | 40-hex oid |
| `pack/{owner}/{name}/{packid}` | raw `.idx` bytes |
| `auth/token/{token}` | owner |
| `auth/sshkey/{sha256-fp}` | owner |

Ref listing = SlateDB prefix scan on `ref/{owner}/{name}/`.

### S3 keys
- `objects/{owner}/{name}/loose/{sha}` — zlib-compressed object, identical to `.git/objects` bytes.
- `objects/{owner}/{name}/pack/{packid}.pack` — packfile as pushed (thin packs resolved before upload).
- `slatedb/` — SlateDB’s own prefix.

`packid` = sha of the pack (git convention).

### Concurrency
One `tokio::sync::Mutex` per repo (in-process map) around receive-pack ref updates. `ponytail:` per-process lock; add SlateDB CAS/leases when running >1 instance.

## Modules
```
src/
  main.rs        config (env: S3 bucket/region/endpoint, listen addrs, host keys), start http+ssh
  store.rs       ObjectStore: get/has/put_loose/put_pack over S3 + slatedb pack idx; small in-memory LRU
  refs.rs        get/set/list refs, repo metadata (slatedb)
  auth.rs        token + sshkey lookup, authorize(owner, repo, write: bool)
  pktline.rs     encode/decode pkt-line, sideband
  protocol/
    upload.rs    v2 ls-refs + fetch: negotiation, object walk, pack building
    receive.rs   receive-pack: read commands + pack, index, store, connectivity check, ref update
  http.rs        axum routes /{owner}/{name}.git/{info/refs,git-upload-pack,git-receive-pack}
  ssh.rs         russh: pubkey auth, exec "git-upload-pack|git-receive-pack '<path>'"
```
Only `ObjectStore` gets a trait-free concrete struct; no trait until a second backend exists.

## Flows

### Push (receive-pack)
1. Read ref-update commands; authorize write.
2. Stream pack to temp file; `gix-pack` index it (resolves thin pack against `ObjectStore`).
3. ≤100 objects → unpack each to loose (S3 put, parallel). Else → S3 put `.pack`, slatedb put idx.
4. Connectivity check: walk each new tip, every reachable object must be `has()`; abort on failure (S3 uploads are orphaned, harmless).
5. Under repo lock: for each command verify old oid still matches, then set. Report `ok`/`ng` per ref.

### Fetch (upload-pack, protocol v2 only)
1. `ls-refs`: from slatedb, honour `ref-prefix`, `symrefs` (HEAD → default branch), `peel`.
2. `fetch`: wants/haves; ack haves that exist; walk from wants stopping at haves; stream a new pack (undeltified, `gix-pack` writer) over sideband. `ponytail:` no deltas/pack reuse; large clones will be slow and fat — add pack reuse when it hurts.
3. Object reads: loose → S3 GET; packed → find via idx (slatedb, cached in memory) → S3 range GET → decode (delta chains resolved recursively). LRU cache of decoded objects (bounded by bytes).

### Auth
HTTP basic `x:<token>` or SSH pubkey → owner. The username is checked, not ignored: on the git and api listeners it must be either `x` (git's placeholder, which every token-based git URL uses) or the owner the token resolves to; the registry listener requires the owner, because `docker login` always has a real username to send. A mismatch is a 401, never a silent downgrade to anonymous. Reads and writes require `owner == {owner}` in path. Unauthenticated → 401 / SSH reject. Tokens/keys managed by a tiny CLI subcommand: `kloudlite-git admin add-token <owner>`, `add-key <owner> <pubkey-file>`, `create-repo <owner>/<name>`.

### Errors
Protocol errors → `ERR` pkt-line or per-ref `ng`. Storage errors before ref update abort the whole push. Nothing is written to refs unless all objects are stored.

## Testing
- Unit: pktline, idx lookup, delta resolution.
- Integration: server with `object_store::memory::InMemory` + slatedb in-memory; run real `git` CLI clone/push/fetch/force-push/branch-delete against HTTP; one SSH smoke test.

## Build order
1. store + refs + loose objects
2. pktline + upload/receive + HTTP; end-to-end clone/push
3. pack storage + reads
4. SSH
5. auth + admin CLI
