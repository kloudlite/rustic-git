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

Workspace layout: `crates/{core,storage,gitbase,pulls,app,git,registry,api,workspaces}` are the
library crates; `bins/{server,api,worker,agent,gateway,kl}` build the six binaries (`rustic-git`,
`rustic-git-api`, `rustic-git-worker`, `rustic-git-agent` — the agent is root-only and runs as a
DaemonSet, one per btrfs-capable node, see "Workspaces and environments" — `rustic-git-gateway`,
the workspace SSH tunnel, and `kl`, the user CLI, which is built by `kl.yml` and never deployed);
the root package is `tests/`'s host only, not a facade.

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
- **The same rule governs the CRDs' `rustic-git.io/owner` and `/kind` labels.** `spec.owner` is the
  truth; the labels are a view of it, and exist only because label selectors are indexed by the API
  server while an arbitrary spec field is not (adding a `selectableFields` entry per query axis is
  how a CRD becomes a database). `/v1` stamps them at create and the node controller RE-STAMPS them
  on every reconcile (`heal_labels`), so an object written by any other path — a restored backup, a
  migration, an operator with kubectl — becomes listable rather than being owned correctly and
  invisible forever. Never authorize on a label; `may_act_on` reads `spec.owner`.
- **The `events` Redis stream (`crates/storage/src/events.rs`) is a nudge for the worker and a view for the
  activity feed, never the record.** Every consumer that matters keeps a fallback that doesn't
  depend on it (the owner's periodic check/announce beats in `bins/server/src/lanes.rs`) — verified
  to still work with Redis entirely down. The one exception is deliberate: the PR half of the
  activity feed is stream-only (`feed.rs`, "no fallback here on purpose"), so with Redis down the
  feed goes quiet on PR events and keeps only `repo_created`.

## PR merges live in the worker, not the server

The owning node only RECORDS merge state (claim/outcome/mergeability — three peer-only routed
endpoints in `bins/server/src/browse_api/pulls.rs`) and re-announces stranded jobs on a 15s beat
(`announce_stranded_merges` in `bins/server/src/lanes.rs`). The actual merge runs in `rustic-git-worker` using the real
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
second, unrelated control plane: btrfs-backed dev workspaces and multi-service environments,
separate from git storage — but sharing the registry namespace with container images: a
workspace/environment's pushed state lives as `vol/{owner}/{id}` in the SAME
`bins/server/src/vol_agent.rs` surface that owns `img/{owner}/{name}`, addressed by the server
tier (`rustic-git`), not `bins/api`.

**Kubernetes is the reconcile substrate, and the CRDs are the source of truth.**
`crates/workspaces/src/crd.rs` defines `Volume`/`Workspace`/`Environment` in
`rustic-git.io/v1alpha1`, all cluster-scoped: `/v1` on `bins/api` writes **spec** (desired), each
node's controller writes **status** (observed) through the `/status` subresource, and RBAC plus
the ValidatingAdmissionPolicy in `deploy/k3s/agent-admission.yaml` — not convention — is what
stops a controller editing desired state: the agent's ClusterRole (`deploy/k3s/agent-rbac.yaml`,
whose header table IS the role) keeps `patch` on the main resources only for labels, finalizers
and the two spec fields a parent's reconciler copies into its own child (`Volume.spec.restoreTo`,
and `Volume.spec.quotaGb` on the home volume an `OwnerBinding` owns),
and the policy refuses it any other spec change. Apply both files. There is no job queue, no lease,
no agent registration and no long poll: `/v1` writes ONE unplaced object and establishes no facts
about it — the node controllers CLAIM it (a guarded write of `status.nodeName`, remembered in
`status.compatibleNodes`), so two nodes can never contend for the same subvolume and the API never
places anything. `crd::Volume` is separate from `Workspace`/`Environment` on purpose — both own
exactly one btrfs subvolume with identical semantics — and it is a CHILD: the parent's controller
creates it with an ownerReference, so deleting the parent is the whole delete. Containers live in
a namespace per owner or environment (`crd::ws_namespace` → `ws-{owner}` / `wt-{owner}-…` for a
team, `env-{id}`): a workspace is one bare Pod, an environment's services are StatefulSets.
`desiredState: Stopped` deletes the workspace pod (and, for an environment, its StatefulSets once
the stop push has landed); the controller re-reads spec on every reconcile, which is how a stop
survives a node reboot. Service-to-service DNS comes from CoreDNS, so `mongodb://db:27017`
resolves inside an environment's namespace. `model::validate_mount` still runs on every mount and
is still load-bearing — a hostPath source escapes just as a bind source did, and the API server
will happily mount `/` if we ask it to.

Cosmos DB (`crates/workspaces/src/cosmos.rs`; `store::MemStore` in-process for dev/tests) now
holds ONLY cross-cluster `Region` metadata — `bins/api` is its only writer, via `/v1/regions`.
Where a CRD and Cosmos could disagree about a workspace, the CRD wins, always. Snapshot bytes (btrfs send streams, block images) go to per-region Azure blob storage, keyed
`blobs/{owner}/{algo}/{hex}` — content-addressed, so nothing scopes them to one test run or one
region deletion; that is deliberate (see `tests/ws_e2e.sh`'s cleanup comments).

Four verbs, no separate commit step: `push` is the single mutating verb — `/v1` writes a
`SnapshotRequest` and the owning node fulfils it: snapshot + upload + register + move the `main`
ref, atomically, with an optional message (`GET /v1/volumes/{name}/history|refs` on `bins/api`
reads the result back as a label list of `done` SnapshotRequests, not from the registry). A
workspace created with `repo`/`branch` is seeded by an init container that clones it over SSH with
the owner's platform key, inside the workspace pod itself — no credential Secret is minted for it.
There is no user-facing un-pushed state; internally
`push` still stages a local RO snapshot before uploading it (the split survives only as a
crash-recovery seam — a push that dies mid-flight leaves the stage files and an internal
`unpushed` mark so a retried push picks them up, never re-snapshotting stray data or losing it).
`clone` (`POST /v1/workspaces/{id}/clone`, the one local-copy verb — "fork" appears nowhere
user-facing) is local-first when the source is materialized on the same pool
(`Engine::clone_local_snapshot`, which works even on a source that has never pushed at all,
running or not); its registry-history fallback (`inherit`) always grafts onto the source's last
PUSHED history. `restore` (`POST /v1/workspaces/restore`) instead grafts onto
an explicit past **snapshot** — a PUSHED commit record, named by id. The agent
(`rustic-git-agent`, privileged, one pod per btrfs-capable node) is a controller, not a worker:
it watches its own node's objects and converges them (`bins/agent/src/controller.rs`), and its
identity is `$NODE_NAME` from the downward API, its liveness the DaemonSet's own probe. It still
reaches the SERVER tier (`WS_REGISTRY_URL`, not `bins/api`) for the `vol/{owner}/{id}` registry
surface — commit records and ref moves — and that is the only thing it calls over HTTP.
Stopping an environment always pushes its own subvolume first, and the Deployment deletes are
gated on the push having LANDED rather than merely been requested (`apply_environment`'s
`DesiredState::Stopped` arm) — the one place push happens without an explicit `/push` call.

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

CI builds images tagged with the commit SHA on push to master — **only if that commit's test job
passed** (`image.yml`'s image job `needs: [build, test]`), so a red commit has no package at all
and a repin to it is an ImagePullBackOff, not a bad deploy. `web.yml` only runs when `web/**`
changed, so the two images do NOT move in lockstep; pin each yaml to the last SHA that actually
built that image. Flow: push → wait for the run → `deploy/pin.sh <sha> [web-sha]` (rewrites every
pin in `deploy/` — server, api, worker, agent, gateway from one SHA, web from the other — and
refuses a SHA with no package) → commit → `deploy/roll.sh` (leader first, wait, then srv/api/worker
and web; the k3s side is applied by hand per `deploy/k3s/README.md`). Never `kubectl apply` the
leader and srv files in one command: the leader's restart overlapping the first srv ordinal's
re-claim is the window in which claims fail, which is why the leader lives in its own file. The
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
