# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```sh
cargo test                                   # full suite (unit + tests/*.rs integration)
cargo test --test registry_blobs             # one integration test file
cargo test --test registry_http some_name    # one test by name
cargo clippy --lib                           # NOTE: --all-targets -D warnings fails with ~13
                                             # PRE-EXISTING errors (objects.rs, browse_api.rs,
                                             # protocol/upload.rs). The bar is: no NEW warnings
                                             # in files you touch.
./tests/registry_e2e.sh                      # real docker push/pull round trip; exit 77 = the
                                             # docker half was skipped (no daemon) — not a pass

cd web && bun install
bun run dev / lint / typecheck / build       # turborepo; app lives in web/apps/web
```

Run a server locally without S3: `RUSTIC_GIT_S3_URL=file://./x` (or `mem://`, lost on exit).
Local scratch (host key, cache) defaults under `./.local/`, which is git-ignored.

## The one invariant everything hangs off

One SlateDB database per repo, and **exactly one node may have it open**. The routing middleware
in `src/http.rs` (`repo_of` → `route_inner`) derives an ownership key from the URL **before
authentication** and refuses anything it cannot route, because opening a database on the wrong
node fences the legitimate owner (a `Closed error: detected newer DB client` in logs means this
happened). Pod `rustic-git-leader-0` is the leader by *name* (no election; set explicitly via
`RUSTIC_GIT_LEADER`, and every pod must agree); it alone writes the ownership map. It runs in its
own StatefulSet and holds no repositories — those live on `rustic-git-srv-{0..N}`. When adding any
route that touches a per-repo/per-image database, it must route —
`BROWSE_TAILS` in `src/http.rs` is the contract, and `every_browse_route_is_routable` holds the
router and the middleware together. A handler that only reads the shared object store may be
served on any node (that is why `/api/{owner}/images` and `_catalog` are exceptions).

## Two namespaces, one server

- Git repos: DB at `repo/{owner}/{name}`, routing key `{owner}/{name}`.
- Container images (OCI registry, `/v2/...`): DB at `repo/img/{owner}/{name}`, routing key
  `img/{owner}/{name}` (`src/registry/`). `api`, `v2`, `img` are reserved owner names so the two
  keyspaces cannot collide. An image is NOT tied to a repo of the same name.

Registry layout in the object store: blobs `blobs/{owner}/{algo}/{hex}` (per-owner, shared
across that owner's images), manifest bytes `manifests/{owner}/{name}/{algo}/{hex}`. Tags,
upload sessions, referrer rows, pull counters live in the image's own DB (single writer ⇒
atomic tag updates).

## Load-bearing rules (violations have all been real bugs)

- **Only two things ever delete a blob**: an explicit client `DELETE /v2/.../blobs/{digest}`,
  and the GC sweep (`src/registry/gc.rs`) — never a manifest path, because siblings share
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
- **The `events` Redis stream (`src/events.rs`) is a nudge for the worker and a view for the
  activity feed, never the record.** Every consumer keeps a fallback that doesn't depend on it
  (the worker's periodic `pull_to_check` sweep, the feed's `pulls_across` fallback) — verified
  to still work with Redis entirely down.

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
can 500 once (known fenced-handle gap). The registry hostname (DNS-only, TLS on the ingress) and the app
hostname are different ingresses with different TLS assumptions — read the comments on both
Ingress objects before touching them.

## House style

Comments explain WHY, never what; match the density of `src/http.rs`. Deliberate shortcuts are
marked `// ponytail: <ceiling and upgrade path>` — keep the marker when editing near one.
Commit subjects are imperative sentence case with no tool attribution. Design docs and plans
live in `docs/superpowers/`; the README's deep sections (ownership, write throughput, container
images) are accurate and worth reading before touching those areas.
