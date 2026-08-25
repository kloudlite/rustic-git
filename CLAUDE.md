# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```sh
cargo test                                   # workspace: every crate's unit tests plus tests/*.rs
                                             # integration suite (the root package, `rustic-git-tests`,
                                             # is a near-empty lib that only hosts tests/)
cargo test --test registry_blobs             # one integration test file — still runs from the root
cargo test --test registry_http some_name    # one test by name
cargo clippy --workspace -- -D warnings      # CI gates on this (image.yml test job): every lib and
                                             # bin, test targets excluded; --all-targets still has
                                             # pre-existing lints in test targets — the bar there is
                                             # no NEW warnings in files you touch.
./tests/registry_e2e.sh                      # real docker push/pull round trip; exit 77 = the
                                             # docker half was skipped (no daemon) — not a pass
./tests/ws_e2e.sh                            # real server+api+agent+Cosmos+Azure+btrfs workspaces
                                             # round trip against a k3s cluster (still three
                                             # binaries — the agent is a controller now, not a
                                             # poller — and rustic-git-api serves /v1/*);
                                             # exit 77 = a prerequisite (root-capable btrfs, a
                                             # reachable cluster with the CRDs installed,
                                             # COSMOS_*/AZURE_* env) was missing — needs a Linux VM
                                             # with btrfs and k3s, not this Mac

cd web && bun install
bun run dev / lint / typecheck / build / test # turborepo; app in web/apps/web; test = bun test
                                             # (*.test.ts are excluded from tsc — no bun-types)
```

Run a server locally without S3: `RUSTIC_GIT_S3_URL=file://./x` (or `mem://`, lost on exit).
Local scratch (host key, cache) defaults under `./.local/`, which is git-ignored.

Workspace layout: `crates/{core,storage,gitbase,pulls,app,git,registry,api}` are the library
crates; `bins/{server,api,worker}` build the three deployed binaries (`rustic-git`,
`rustic-git-api`, `rustic-git-worker`); the root package is `tests/`'s host only, not a facade.

## The one invariant everything hangs off

One SlateDB database per repo, and **exactly one node may have it open**. The routing middleware
in `bins/server/src/router/route.rs` (`repo_of` → `route_inner`) derives an ownership key from the URL **before
authentication** and refuses anything it cannot route, because opening a database on the wrong
node fences the legitimate owner (a `Closed error: detected newer DB client` in logs means this
happened). Pod `rustic-git-leader-0` is the leader by *name* (no election; set explicitly via
`RUSTIC_GIT_LEADER`, and every pod must agree); it alone writes the ownership map. It runs in its
own StatefulSet and holds no repositories — those live on `rustic-git-srv-{0..N}`. When adding any
route that touches a per-repo/per-image database, it must route —
`BROWSE_TAILS` in `bins/server/src/router/route.rs` is the contract, and `every_browse_route_is_routable` holds the
router and the middleware together. A handler that only reads the shared object store may be
served on any node (that is why `/api/{owner}/images` and `_catalog` are exceptions).

## Two namespaces, one server

- Git repos: DB at `repo/{owner}/{name}`, routing key `{owner}/{name}`.
- Container images (OCI registry, `/v2/...`): DB at `repo/img/{owner}/{name}`, routing key
  `img/{owner}/{name}` (`crates/registry/src/`). `api`, `v2`, `img` are reserved owner names so the two
  keyspaces cannot collide. An image is NOT tied to a repo of the same name.

Registry layout in the object store: blobs `blobs/{owner}/{algo}/{hex}` (per-owner, shared
across that owner's images), manifest bytes `manifests/{owner}/{name}/{algo}/{hex}`. Tags,
upload sessions, referrer rows, pull counters live in the image's own DB (single writer ⇒
atomic tag updates).

## Load-bearing rules (violations have all been real bugs)

- **Only two things ever delete a blob**: an explicit client `DELETE /v2/.../blobs/{digest}`,
  and the GC sweep (`crates/registry/src/gc.rs`) — never a manifest path, because siblings share
  layers. The sweep is keep-biased: any uncertainty (unreadable manifest) aborts it.
- **Manifest bytes are stored and returned verbatim.** The digest is over those exact bytes;
  parse to read a field, never re-emit.
- **`Digest::parse` is the only way a path segment becomes an object-store key** (sha256 or
  sha512, lowercase hex, exact length). Upload-session uuids are validated the same way.
- Every `/v2` error is the OCI envelope via `registry::oci_err`; auth flows through
  `registry::auth::allow` (Basic and Bearer both; anonymous ≠ invalid credential — an
  anonymous token from `/v2/token` must keep working for public pulls).
- Registry blob routes have their own body limit (`max_layer`, default 10 GiB) separate from
  the git `max_body` (2 GiB); manifests have a third. Check which limit applies before assuming
  a 413 is the handler's.
- The browse API mounts on the **peer listener only**; the public listener 404s `/api/`.
  Credentials live as plain object-store keys (any node authenticates), not in SlateDB.
- **Markers under `index/` are views for listings, never authorization.** Owning nodes write them
  and reconcile their visibility; the GC worker reconciles their structure.
- **The `events` Redis stream (`crates/storage/src/events.rs`) is a nudge for the worker and a view for the
  activity feed, never the record.** Every consumer keeps a fallback that doesn't depend on it
  (the owner's periodic check/announce beats in `bins/server/src/lanes.rs`, the feed's `pulls_across` fallback) — verified
  to still work with Redis entirely down.

## PR merges live in the worker, not the server

The owning node only RECORDS merge state (claim/outcome/mergeability — three peer-only routed
endpoints in `bins/server/src/browse_api/pulls.rs`) and re-announces stranded jobs on a 15s beat
(`App::announce_stranded_merges`). The actual merge runs in `rustic-git-worker` using the real
`git` binary (`crates/pulls/src/merge_worker.rs`): bare cache under the worker's cache dir, fetch/push over
the peer listener with `-c http.extraHeader` peer auth, `merge-tree --write-tree` for
merge/squash, a throwaway worktree for rebase, `push --force-with-lease` against the oid the
merge was computed from. Traps that were all real: the server speaks upload-pack protocol v2
ONLY, and libgit2 has no v2 — git2/libgit2 cannot fetch from this server; pods have no git
identity, so every commit-writing git call must set GIT_COMMITTER_*/GIT_AUTHOR_* env; a retried
squash is caught by merged-tree == base-tree, not by ancestry or the lease. The `local()` vs
`networked()` split in `merge_worker.rs` is what keeps the peer secret out of error messages —
never format a networked argv into anything.

Fetch packs are built with `TreeAdditionsComparedToAncestor` plus a full-tree second pass for
merge commits — gix-pack drops all-but-last-parent additions on a merge
(GitoxideLabs/gitoxide#2935); delete the workaround in `crates/git/src/protocol/upload/pack.rs` when that fixes.

## Workspaces and environments

`crates/workspaces` + `bins/agent` (`rustic-git-agent`) + `bins/api` (`rustic-git-api`) add a
second, unrelated control plane: btrfs-backed dev workspaces and docker-compose environments,
separate from git storage — but sharing the registry namespace with container images: a
workspace/environment's pushed state lives as `vol/{owner}/{id}` in the SAME
`bins/server/src/vol_agent.rs` surface that owns `img/{owner}/{name}`, addressed by the server
tier (`rustic-git`), not `bins/api`. Metadata (`Workspace`/`Environment`/`Region`/`Job` docs) lives
in Cosmos DB (`crates/workspaces/src/cosmos.rs`; `store::MemStore` in-process for dev/tests), not
SlateDB — the api bin is the only writer of `/v1/regions`, `/v1/workspaces`, `/v1/environments`.
Snapshot bytes (btrfs send streams, block images) go to per-region Azure blob storage, keyed
`blobs/{owner}/{algo}/{hex}` — content-addressed, so nothing scopes them to one test run or one
region deletion; that is deliberate (see `tests/ws_e2e.sh`'s cleanup comments).

Four verbs, no separate commit step: `push` is the single mutating verb — snapshot + upload +
register + move the `main` ref, atomically, with an optional message
(`GET /v1/volumes/{name}/history|refs` on `bins/api` reads the result back, proxied through
`RegistryClient` against the server tier). There is no user-facing un-pushed state; internally
`push` still stages a local RO snapshot before uploading it (the split survives only as a
crash-recovery seam — a push that dies mid-flight leaves the stage files and an internal
`unpushed` mark so a retried push picks them up, never re-snapshotting stray data or losing it).
`clone` (`POST /v1/workspaces/{id}/clone`, the one local-copy verb — "fork" appears nowhere
user-facing) is local-first when the source is materialized on the same pool
(`Engine::clone_local`, which works even on a source that has never pushed at all), else a
two-phase live copy (`Engine::clone_running`); its registry-history fallback always grafts onto
the source's last PUSHED history. `restore` (`POST /v1/workspaces/restore`) instead grafts onto
an explicit past **snapshot** — a PUSHED commit record, named by id. Agents (`rustic-git-agent`,
one per btrfs-capable box, root-only) never receive pushed work: they long-poll `GET
/vol-agent/work` against the SERVER tier (`WS_REGISTRY_URL`, not `bins/api`) and lease a queued
`Job` via CAS, so a dead agent's work just sits queued for the next poller — no separate failover
path to get wrong. `env stop` (`EnvDown`) always pushes the environment's own subvolume
atomically before tearing the environment's Deployments down (see `bins/agent/src/lib.rs`'s `EnvUp`/`EnvDown` arms) —
the one place push happens without an explicit `/push` call.

## Web app

Next.js app router in `web/apps/web` (its own `CLAUDE.md`/`AGENTS.md` there warns the installed
Next.js differs from training data — read `node_modules/next/dist/docs/` when unsure). One shell
(`components/app/app-shell.tsx`) renders all chrome; `shell-nav.tsx`'s `place()` classifies the
URL as org / repo / image and picks the tab row — reserved names in `store::RESERVED_REPO_NAMES`
are what make that unambiguous. Copy existing siblings, not new patterns: `repo-list.tsx` for
filterable lists, repo `settings/` for destructive actions, `lib/time.ts` for size/date
formatting. Tokens over raw Tailwind colors; `--radius: 0` — sharp corners everywhere.
Editor TS diagnostics here are frequently stale; trust `bunx tsc --noEmit -p apps/web/tsconfig.json`.

## Deploying

CI builds images tagged with the commit SHA on push to master — but `web.yml` only runs when
`web/**` changed, so the two images do NOT move in lockstep; pin each yaml to the last SHA that
actually built that image. Flow: push → wait for the run → edit the image tags in
`deploy/rustic-git.yaml` / `deploy/rustic-git-web.yaml` → commit → `kubectl apply`. The
StatefulSet roll moves DB ownership between nodes; the first registry request to a moved image
can 500 once (known fenced-handle gap). The registry hostname (Cloudflare-proxied — verify with `dig` before touching ssl-redirect) and the app
hostname are different ingresses with different TLS assumptions — read the comments on both
Ingress objects before touching them. The worker liveness probe counts per-lane heartbeat files
and the web probes hit `/api/health`, so a yaml roll must never outrun its image repin. The
`rustic-git-jwt` Secret is required (pods fail closed without it), and Rust pods run as uid 1001
with a read-only root — anything new that writes to disk needs a mount.

## House style

Comments explain WHY, never what; match the density of `bins/server/src/router/route.rs`. Deliberate shortcuts are
marked `// ponytail: <ceiling and upgrade path>` — keep the marker when editing near one.
Commit subjects are imperative sentence case with no tool attribution. Design docs and plans
live in `docs/superpowers/`; the README's deep sections (ownership, write throughput, container
images) are accurate and worth reading before touching those areas.
