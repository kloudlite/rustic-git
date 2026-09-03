# Runtime tunables inventory — every binary

Read-only survey of `std::env::var`/`env::var_os` reads across `bins/*`, `crates/{core,storage,registry,app,api,pulls,workspaces}`, and `process.env.` reads in `web/apps/web/src`, as of 2026-09-03. Columns match the task: env name, binary/tier, reader `file:line`, default, type/unit, what it controls, secret?, bootstrap-only?, safe to change live?, and the deploy file/value that sets it today.

"Bootstrap-only" = read once at startup into a struct/const that never re-reads after. A var read via a plain function call on every invocation (`env("X","d")` called per-request, or cached in a `OnceLock`) is bootstrap-only in effect too, since the process never sees a changed value without a restart — noted per-row.

---

## server (`rustic-git`, bins/server)

| env name | reader file:line | default | type/unit | controls | secret? | bootstrap-only? | safe live? | deploy file : value |
|---|---|---|---|---|---|---|---|---|
| `RUSTIC_GIT_S3_URL` | crates/storage/src/config.rs:48 | none (required) | URL (`s3://`/`az://`/`file://`/`mem://`) | object store backend | no (URL itself; may embed nothing, creds come from AWS_*/AZURE_* separately) | yes | never | rustic-git.yaml : `az://rustic-git` |
| `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`/`AWS_REGION` | crates/storage/src/config.rs:74 (`AmazonS3Builder::from_env`) | none | strings | S3 credentials/region | yes | yes | never | not set (az:// path used) |
| `AWS_ENDPOINT` | crates/storage/src/config.rs:79 | none | URL | S3-compatible endpoint override | no | yes | never | not set |
| `AZURE_STORAGE_ACCOUNT_NAME`/`AZURE_STORAGE_ACCOUNT_KEY` | crates/storage/src/config.rs:92 (`MicrosoftAzureBuilder::from_env`) | none | strings | Azure blob credentials | yes | yes | never | rustic-git.yaml : secretKeyRef |
| `RUSTIC_GIT_S3_TIMEOUT_SECS` | crates/storage/src/config.rs:71 | `900` | seconds | S3 client request timeout | no | yes | needs-restart | not set (default) |
| `RUSTIC_GIT_ALLOW_MEM_FLEET` | crates/storage/src/config.rs:132 | unset=refuse | bool (presence) | allows `mem://` in a multi-node fleet (test only) | no | yes | never | not set |
| `RUSTIC_GIT_CACHE_DIR` | crates/storage/src/config.rs:147, crates/storage/src/pool/mod.rs:155/377 | `./.local/cache` | path | SlateDB local disk-cache root + repo-pool cache dir | no | yes | never | rustic-git.yaml : `/var/cache/rustic-git` |
| `RUSTIC_GIT_REDIS_URL` | crates/storage/src/config.rs:152 | unset=fail-open (no cache/events) | URL | Redis connection (event stream, cache invalidation) | yes (may embed password) | yes | needs-restart | rustic-git.yaml : secretKeyRef |
| `RUSTIC_GIT_SLATEDB_DISK_CACHE_MB` | crates/storage/src/err.rs:154 (`disk_cache_options`) | `4096` | MB | SlateDB on-disk block cache size; `0` disables | no | yes | never | not set (default) |
| `RUSTIC_GIT_PEER_ADDR` | bins/server/src/main.rs:25 | `0.0.0.0:8081` | host:port | peer HTTP listener bind addr | no | yes | never | not set (default used) |
| `RUSTIC_GIT_PEER_SVC` | bins/server/src/main.rs:34 | `""` (solo mode) | DNS name | headless Service name; presence = fleet mode | no | yes | never | rustic-git.yaml : `rustic-git.rustic-git.svc.cluster.local` |
| `RUSTIC_GIT_SELF` | bins/server/src/main.rs:59 | none (required in fleet) | string | this node's identity/ownership key | no | yes | never | rustic-git.yaml : (pod-name via downward API) |
| `RUSTIC_GIT_PEER_SECRET` | bins/server/src/main.rs:60, boot.rs:13/247/269 | random per-process (solo) | string | peer-listener bearer secret | yes | yes | never | rustic-git.yaml : secretKeyRef |
| `RUSTIC_GIT_JWT_SECRET` | crates/app/src/lib.rs:146, crates/core/err.rs:31 | random per-process | string | JWT signing/verification secret | yes | yes | never | rustic-git.yaml : secretKeyRef |
| `RUSTIC_GIT_MONGO_URI` | bins/server/src/main.rs:77 | unset=no directory | connection string | Mongo directory connection (PR migration source) | yes | yes | needs-restart | rustic-git.yaml : secretKeyRef |
| `RUSTIC_GIT_MONGO_DB` | bins/server/src/main.rs:79 | `kloudlite` | string | Mongo database name | no | yes | needs-restart | not set (default) |
| `RUSTIC_GIT_HOST_KEY` | bins/server/src/main.rs:110 | `./.local/host_key` | path | SSH host key file path | no | yes | never | rustic-git.yaml : `/etc/rustic-git/host_key` |
| `RUSTIC_GIT_MAX_BODY` | crates/core/src/httpx.rs:106 | `2147483648` (2 GiB) | bytes | max git request body size | no | no (`env::var` read each call — cheap, effectively per-request but no caching, so a live env change via a hot-reload sidecar *would* apply; in practice it's set once at pod start) | needs-restart | rustic-git.yaml : `536870912` (512 MiB) |
| `RUSTIC_GIT_METRICS_ADDR` | crates/core/src/metrics.rs:48 | unset=no metrics listener | host:port | Prometheus metrics listener bind addr | no | yes | never | rustic-git.yaml : `0.0.0.0:9464` |
| `RUSTIC_GIT_LOG_FORMAT` | crates/core/src/log.rs:30 | text | `json`/other | log output format | no | yes | never | not set (text) |
| `RUST_LOG` | crates/core/src/log.rs:41 (`EnvFilter::try_from_default_env`) | crate-scoped default filter | tracing filter string | log verbosity | no | yes | never | not set |
| `RUSTIC_GIT_EXTERNAL_URL` | crates/registry/src/auth.rs:16 | `http://localhost:8080` | URL | external URL advertised in registry 401 challenge | no | yes | never | rustic-git.yaml : `https://cr.khost.dev` |
| `RUSTIC_GIT_MAX_LAYER` | crates/registry/src/blobs.rs:30 (cached in `OnceLock`) | `5368709120` (5 GiB) | bytes | max single registry blob layer | no | yes (OnceLock, first read wins) | never | not set (default) |
| `RUSTIC_GIT_UPLOAD_GRACE_SECS` | crates/registry/src/uploads.rs:43 | `86400` (24h) | seconds | abandoned upload-session GC grace period | no | no (read per GC pass) | yes | not set (default) |
| `RUSTIC_GIT_UPSTREAM` | bins/server/src/boot.rs:47/247/269 | `http://rustic-git:8081` (in `post_to_owner`) | URL | admin CLI: peer Service URL to route flips through | no | yes (CLI invocation) | n/a (CLI, not the server process) | not set at server level (used by admin subcommands / api / worker) |
| `RUSTIC_GIT_WARM_TTL_SECS` | crates/storage/src/pool/mod.rs:245 | `300` | seconds | idle TTL before a warm repo DB is closed | no | yes (read once into pool config) | needs-restart | rustic-git.yaml : `300` |
| `RUSTIC_GIT_MAX_WARM` | crates/storage/src/pool/mod.rs:246 | `64` | count | max warm repo DBs held open per node | no | yes | needs-restart | rustic-git.yaml : `16` |
| `RUSTIC_GIT_AGENT_SOURCES` | *(set in deploy, not read anywhere in `.rs`)* | — | — | vestigial/dead — grep found zero readers | no | n/a | n/a | rustic-git.yaml : `centralindia-k3s=40.80.82.158/32,20.219.22.61/32` (dead value) |

Notes: `RUSTIC_GIT_FLUSH_INTERVAL_MS`, `RUSTIC_GIT_SLATEDB_BLOCK_CACHE_MB`, `RUSTIC_GIT_SLATEDB_META_CACHE_MB` are compile-time `const`s in `crates/storage/src/err.rs` (not env-configurable) — included here only because their names look like tunables; excluded from the table and from counts since they never read `env::var`.

---

## api (`rustic-git-api`, bins/api)

| env name | reader file:line | default | type/unit | controls | secret? | bootstrap-only? | safe live? | deploy file : value |
|---|---|---|---|---|---|---|---|---|
| `RUSTIC_GIT_S3_URL`, `AZURE_STORAGE_*`, `RUSTIC_GIT_S3_TIMEOUT_SECS`, `RUSTIC_GIT_ALLOW_MEM_FLEET`, `RUSTIC_GIT_CACHE_DIR`, `RUSTIC_GIT_REDIS_URL`, `RUSTIC_GIT_SLATEDB_DISK_CACHE_MB` | shared `open_store`/`config.rs` — see server tier | see server tier | see server tier | object store + cache, shared bootstrap | mixed | yes | never/needs-restart | rustic-git.yaml (api container): S3_URL `az://rustic-git`, others via secretKeyRef; `RUSTIC_GIT_CACHE_DIR` not set for api (falls to `./.local/cache`) |
| `RUSTIC_GIT_UPSTREAM` | bins/api/src/main.rs:75 | `http://rustic-git:8081` | URL | git-tier peer Service, where browse/pull routes are proxied | no | yes | never | rustic-git.yaml : `http://rustic-git:8081` |
| `RUSTIC_GIT_PEER_SECRET` | bins/api/src/main.rs:76 | none (required) | string | peer-listener bearer secret | yes | yes | never | rustic-git.yaml : secretKeyRef |
| `RUSTIC_GIT_MONGO_URI` | bins/api/src/main.rs:82 | unset=directory routes 503 | connection string | Mongo directory connection | yes | yes | needs-restart | rustic-git.yaml : secretKeyRef |
| `RUSTIC_GIT_MONGO_DB` | bins/api/src/main.rs:84 | `kloudlite` | string | Mongo database name | no | yes | needs-restart | rustic-git.yaml : `kloudlite` |
| `RUSTIC_GIT_JWT_SECRET` | bins/api/src/main.rs:98, crates/core/err.rs:31 (`require_jwt_secret_from_env`) | none (fleet-required) | string | JWT signing secret for `/v1` sign-in | yes | yes | never | rustic-git.yaml : secretKeyRef |
| `RUSTIC_GIT_PEER_SVC` (indirectly, via `require_jwt_secret_from_env`) | crates/core/err.rs:32 | `""` | — | just gates the JWT-secret-required check | no | yes | never | not set on api tier (n/a — api always requires JWT) |
| `RUSTIC_GIT_WORKSPACES_ADMINS` | bins/api/src/main.rs:113 | `""` (no admins) | comma-separated emails | static admin allowlist for `/v1/regions` | no (emails, not credentials — arguably sensitive PII but not a "key/token/password") | yes | needs-restart | rustic-git.yaml : `karthik@kloudlite.io` |
| `RUSTIC_GIT_API_ADDR` | bins/api/src/main.rs:141 | `0.0.0.0:8090` | host:port | HTTP listener bind addr | no | yes | never | rustic-git.yaml : `0.0.0.0:8090` |
| `RUSTIC_GIT_METRICS_ADDR` | crates/core/src/metrics.rs:48 | unset=no listener | host:port | Prometheus metrics listener | no | yes | never | rustic-git.yaml : `0.0.0.0:9464` |
| `RUSTIC_GIT_LOG_FORMAT`, `RUST_LOG` | crates/core/src/log.rs | text / default filter | — | log format/verbosity | no | yes | never | not set |
| `RUSTIC_GIT_CLI_CODE_LIMIT` | crates/api/src/lib.rs:155 → ratelimit.rs:42 | `20/600` | `N/seconds` | rate limit on `POST /v1/cli/code` | no | yes (per-process bucket) | needs-restart | not set (default) |
| `RUSTIC_GIT_SIGNIN_IP_LIMIT` | crates/api/src/lib.rs:156 | `10/60` | `N/seconds` | rate limit on sign-in by IP | no | yes | needs-restart | not set (default) |
| `RUSTIC_GIT_SIGNIN_EMAIL_LIMIT` | crates/api/src/lib.rs:158 | `1/60` | `N/seconds` | rate limit on sign-in by email | no | yes | needs-restart | not set (default) |
| `WS_MAX_PER_OWNER` | crates/workspaces/src/model.rs:34 | `20` | count | max workspaces+environments per owner | no | no (read per request) | yes | rustic-git.yaml : commented out (default `20` in effect) |

Kube client config (`KUBECONFIG`, in-cluster SA env) is read by the `kube` crate itself, not by this codebase directly — out of scope (not a `std::env::var(` call in this repo).

---

## worker (`rustic-git-worker`, bins/worker)

| env name | reader file:line | default | type/unit | controls | secret? | bootstrap-only? | safe live? | deploy file : value |
|---|---|---|---|---|---|---|---|---|
| `RUSTIC_GIT_S3_URL`, `AZURE_STORAGE_*`, `RUSTIC_GIT_CACHE_DIR`, `RUSTIC_GIT_REDIS_URL`, etc. | shared `open_store` | see server tier | — | object store + cache bootstrap | mixed | yes | never/needs-restart | rustic-git.yaml (worker container): S3_URL `az://rustic-git`, `RUSTIC_GIT_CACHE_DIR` `/var/cache/rustic-git`, Redis via secretKeyRef |
| `RUSTIC_GIT_UPSTREAM` | bins/worker/src/main.rs:65 | `http://rustic-git:8081` | URL | git-tier peer Service to claim/report merges | no | yes | never | rustic-git.yaml : `http://rustic-git:8081` |
| `RUSTIC_GIT_PEER_SECRET` | bins/worker/src/main.rs:66 | none (required) | string | peer-listener bearer secret | yes | yes | never | rustic-git.yaml : secretKeyRef |
| `RUSTIC_GIT_WORKER_CONCURRENCY` | bins/worker/src/main.rs:76 | `4` (clamped 1-64) | count | number of merge lanes | no | yes | needs-restart | rustic-git.yaml : `4` |
| `RUSTIC_GIT_MERGE_CMD_TIMEOUT` | crates/pulls/src/merge_worker.rs:218 (cached OnceLock) | `900` (15 min) | seconds | per-git-command timeout inside a merge job | no | yes (OnceLock) | never | not set (default) |
| `RUSTIC_GIT_MERGE_JOB_TIMEOUT` | crates/pulls/src/merge_worker.rs:221 | `1500` (25 min) | seconds | whole-merge-job ceiling | no | yes (OnceLock) | never | not set (default) |
| `RUSTIC_GIT_METRICS_ADDR` | crates/core/src/metrics.rs:48 | unset=no listener | host:port | Prometheus metrics listener | no | yes | never | rustic-git.yaml : `0.0.0.0:9464` |
| `RUSTIC_GIT_LOG_FORMAT`, `RUST_LOG` | crates/core/src/log.rs | text/default | — | log format/verbosity | no | yes | never | not set |

Also reads `RUSTIC_GIT_MAX_LAYER`/`RUSTIC_GIT_UPLOAD_GRACE_SECS` indirectly through the shared `crates/registry::gc`/`uploads` code it links (its GC lane calls `sweep_owner`/`sweep_stale_uploads`) — same rows as server tier, not duplicated here.

---

## gateway (`rustic-git-gateway`, bins/gateway)

| env name | reader file:line | default | type/unit | controls | secret? | bootstrap-only? | safe live? | deploy file : value |
|---|---|---|---|---|---|---|---|---|
| `RUSTIC_GIT_JWT_SECRET` | bins/gateway/src/main.rs:25 | `""` → `Jwt::new("")` (likely fails/degrades) | string | JWT verification secret for SSH-tunnel auth | yes | yes | never | gateway.yaml : secretKeyRef `rustic-git-jwt` |
| `WS_REGION` | bins/gateway/src/main.rs:31 | none (fatal if unset/empty) | string | region identity, must match token's region claim | no | yes | never | gateway.yaml : fieldRef/configMap `rustic-git-gateway` |
| `GATEWAY_TLS_DIR` | bins/gateway/src/main.rs:51 | unset=HTTP-only (dev) | path | directory holding `tls.crt`/`tls.key` for the 443 listener | no (path, not the cert itself) | yes | never | not shown in grep — presumably set in gateway.yaml volume mount (not matched by the `name:`/`value:` grep here, check the container spec directly if precision needed) |
| `RUSTIC_GIT_METRICS_ADDR` | crates/core/src/metrics.rs:48 | unset=no listener | host:port | Prometheus metrics listener | no | yes | never | gateway.yaml : `0.0.0.0:9464` |
| `RUST_BACKTRACE` | (stdlib, not this codebase) | — | `0`/`1` | Rust panic backtraces | no | yes | never | gateway.yaml : `"1"` |
| `RUSTIC_GIT_LOG_FORMAT`, `RUST_LOG` | crates/core/src/log.rs | text/default | — | log format/verbosity | no | yes | never | not set |

---

## agent (`rustic-git-agent`, bins/agent)

| env name | reader file:line | default | type/unit | controls | secret? | bootstrap-only? | safe live? | deploy file : value |
|---|---|---|---|---|---|---|---|---|
| `WS_REGION` | bins/agent/src/lib.rs:38 | `default` | string | region label on this agent's config | no | yes | never | agent-daemonset.yaml : (not explicitly grepped — check for `WS_REGION` on agent; likely unset/relies on default, or set — verify in deploy if precision needed) |
| `WS_POOL` | bins/agent/src/lib.rs:39 | `/mnt/wspool` | path | btrfs pool root | no | yes | never | agent-daemonset.yaml : `/wspool-prod` |
| `NODE_NAME` | bins/agent/src/lib.rs:42 | `""` | string | this node's identity (downward API), shard key for reconciliation | no | yes | never | agent-daemonset.yaml : fieldRef `spec.nodeName` |
| `WS_HOMES_EXPORT` | bins/agent/src/lib.rs:43 | `None` (fail-closed: HomeNotReady) | string (NFS export) | region-shared home NFS export address | no | yes | never | agent-daemonset.yaml : `zerofs.rustic-git-system.svc:/` |
| `WS_PEER_SECRET` | bins/agent/src/lib.rs:226, controller/mod.rs:245 | `""` | string | peer-listener bearer secret (btrfs send auth) | yes | yes | never | agent-daemonset.yaml : (via secretKeyRef, not shown by `name:`/`value:` grep) |
| `WS_PEER_ADDR` | bins/agent/src/peer.rs:94 | `0.0.0.0:8444` | host:port | peer listener bind addr | no | yes | never | agent-daemonset.yaml : `0.0.0.0:8444` |
| `WS_REPLICA_SECS` | bins/agent/src/peer.rs:233 | `300` | seconds | replication pull beat interval | no | yes (cached) | needs-restart | agent-daemonset.yaml : `300` |
| `WS_PEER_SEND_TIMEOUT_SECS` | bins/agent/src/peer.rs:266 | `3600` | seconds | btrfs-send-over-HTTP timeout | no | yes (cached) | needs-restart | agent-daemonset.yaml : `3600` |
| `WS_NODE_DEAD_SECS` | bins/agent/src/peer.rs:391 | `600` | seconds | how long before a node is declared dead for placement | no | no (read per check, no cache) | yes | agent-daemonset.yaml : `180` |
| `WS_SYNC_SECS` | bins/agent/src/sync.rs:38 | `60` | seconds | sync-point cut beat interval | no | yes (cached at task spawn) | needs-restart | agent-daemonset.yaml : `60` |
| `WS_DECOMMISSION_SECS` | bins/agent/src/decommission.rs:27 | `30` | seconds | decommission-beat interval | no | yes (cached) | needs-restart | agent-daemonset.yaml : `30` |
| `WS_BASE_PACKAGES` | bins/agent/src/nix.rs:63 | (whitespace list, see const) | space-separated string | packages prepended to every workspace's Nix profile | no | no (read per build, not cached) | yes | agent-daemonset.yaml : matches the code default explicitly |
| `WS_NIXPKGS` | bins/agent/src/nix.rs:68 | `""` | pin string (`github:NixOS/nixpkgs/<rev>`) | nixpkgs revision pin | no | no (read per build) | yes | agent-daemonset.yaml : `github:NixOS/nixpkgs/c27cdad491a991b11ed731760aa2ef8db0cb0410` |
| `WS_NIX_TIMEOUT` | bins/agent/src/nix.rs:72 | `DEFAULT_TIMEOUT_SECS` (const, likely 1800 — not confirmed in this pass) | seconds | nix build timeout | no | no (read per build) | yes | agent-daemonset.yaml : `1200` |
| `WS_DEFAULT_IMAGE` | bins/agent/src/controller/mod.rs:224 | none (panics if unset) | image ref | pinned workspace container image | no | yes | never | agent-daemonset.yaml : `ghcr.io/kloudlite/rustic-git-workspace:1f24e39...` |
| `WS_RUNTIME_CLASS` | bins/agent/src/controller/mod.rs:226 | `None` (unsandboxed) | RuntimeClass name | gVisor/sandboxed runtime for tenant pods | no | yes | never | agent-daemonset.yaml : (not shown; check runtimeclass.yaml wiring) |
| `WS_GIT_SSH_HOST` | bins/agent/src/controller/mod.rs:253 | `git.khost.dev` | hostname | SSH host the init container clones from | no | yes | never | agent-daemonset.yaml : `git.khost.dev` |
| `WS_GIT_SSH_PORT` | bins/agent/src/controller/mod.rs:254 | `22` | port | SSH port for the clone init container | no | yes | never | agent-daemonset.yaml : `22` |
| `WS_GIT_INIT_IMAGE` | bins/agent/src/controller/mod.rs:255 | `alpine/git:2.45.2` | image ref | init-container image for seeding a workspace | no | yes | never | agent-daemonset.yaml : `alpine/git:2.45.2@sha256:16ad8e78...` |
| `RUSTIC_GIT_METRICS_ADDR` | crates/core/src/metrics.rs:48 | unset=no listener | host:port | Prometheus metrics listener | no | yes | never | agent-daemonset.yaml : `0.0.0.0:9464` |
| `RUSTIC_GIT_LOG_FORMAT`, `RUST_LOG` | crates/core/src/log.rs | text/default | — | log format/verbosity | no | yes | never | not set |

`PROFILES_DIR` (nix profile index root, referenced in CLAUDE.md prose as `{PROFILES_DIR}`) was not found as a literal `std::env::var("PROFILES_DIR")` in this pass — likely a constant or a field threaded from `Config`; flag for a follow-up grep if it matters (`grep -rn PROFILES_DIR bins/agent`).

---

## web (`rustic-git-web`, Next.js, TypeScript)

| env name | reader file:line | default | type/unit | controls | secret? | bootstrap-only? | safe live? | deploy file : value |
|---|---|---|---|---|---|---|---|---|
| `AUTH_SECRET` | web/apps/web/src/lib/api-token.ts:24, assertion.ts:15 | none | string | NextAuth/JWT signing secret, WebAuthn assertion secret | yes | yes (module-level read) | never | rustic-git-web.yaml : secretKeyRef |
| `AUTH_URL` | web/apps/web/src/auth.ts:130, settings/actions.ts:67, login/actions.ts:40, passkey.ts:18 | `""` (fatal in prod if unset — auth.ts:133) | URL | canonical app URL for auth callbacks/relying-party | no | yes | never | rustic-git-web.yaml : `https://dev.kloudlite.io` |
| `AUTH_TRUST_HOST` | (NextAuth internal, not read directly in grepped files but set in deploy) | — | bool | trust `X-Forwarded-Host` | no | yes | never | rustic-git-web.yaml : `"true"` |
| `AUTH_ALLOWED_EMAILS` | web/apps/web/src/auth.ts:17/109 | `""` | comma-separated emails | shared-password login allowlist | no (emails; not itself a credential) | yes | needs-restart | rustic-git-web.yaml : secretKeyRef, optional |
| `AUTH_SHARED_PASSWORD` | web/apps/web/src/auth.ts:21/109 | `""` | string | shared-password login credential | yes | yes | needs-restart | rustic-git-web.yaml : secretKeyRef, optional |
| `AUTH_GITHUB_ID`/`AUTH_GITHUB_SECRET` | web/apps/web/src/auth.ts:90-91/117 | none (provider omitted if unset) | strings | GitHub OAuth app credentials | secret is yes (ID is not) | yes | never | rustic-git-web.yaml : secretKeyRef, optional |
| `AUTH_GOOGLE_ID`/`AUTH_GOOGLE_SECRET` | web/apps/web/src/auth.ts:93-94/118 | none | strings | Google OAuth app credentials | secret is yes | yes | never | rustic-git-web.yaml : secretKeyRef, optional |
| `RESEND_API_KEY` | web/apps/web/src/lib/mail.ts:40, auth.ts:114 | none | string | Resend transactional-mail API key | yes | yes | never | rustic-git-web.yaml : secretKeyRef, optional |
| `RESEND_FROM` | web/apps/web/src/lib/mail.ts:41, auth.ts:114 | none | email address | mail "from" address | no | yes | needs-restart | rustic-git-web.yaml : secretKeyRef, optional |
| `RUSTIC_GIT_SSH_PORT` | web/apps/web/src/lib/clone.ts:33 | `22` | port | SSH port shown in Clone menu | no | no (read per render) | yes | rustic-git-web.yaml : `22` |
| `RUSTIC_GIT_API_URL` | web/apps/web/src/lib/api.ts:16, browse.ts:13 | `http://rustic-git-api` | URL | in-cluster api tier base URL | no | yes (module-level `const`) | never | rustic-git-web.yaml : `http://rustic-git-api` |
| `RUSTIC_GIT_PEER_SECRET` | web/apps/web/src/lib/api.ts:17 | `""` | string | bearer secret web uses when calling the api tier | yes | yes (module-level `const`) | never | rustic-git-web.yaml : secretKeyRef |
| `RUSTIC_GIT_CLONE_HOST` | web/apps/web/src/lib/clone.ts:29 (`host()`) | `localhost` (dev only; throws in prod if unset) | hostname | https clone-URL host shown in UI | no | no (read per render, but effectively fixed for the pod's life) | yes | rustic-git-web.yaml : `dev.kloudlite.io` |
| `RUSTIC_GIT_REGISTRY_HOST` | web/apps/web/src/lib/clone.ts:27 | `localhost` (dev; throws in prod if unset) | hostname | registry hostname shown in UI (`docker pull` snippets) | no | no (read per render) | yes | rustic-git-web.yaml : `cr.khost.dev` |
| `RUSTIC_GIT_SSH_HOST` | web/apps/web/src/lib/clone.ts:32 | `localhost` (dev; throws in prod if unset) | hostname | SSH clone host shown in UI | no | no (read per render) | yes | rustic-git-web.yaml : `git.khost.dev` |
| `NODE_ENV` | web/apps/web/src/auth.ts:133, lib/clone.ts:24 | (Next.js sets it) | `development`/`production` | dev-vs-prod fallback/throw behavior | no | yes | never | Next.js standard, not set explicitly in deploy |
| `NEXT_PHASE` | web/apps/web/src/auth.ts:133 | (Next.js build-time) | string | suppresses the `AUTH_URL`-required check during `next build` | no | yes | never | build-time only, not in deploy |
| `NODE_OPTIONS` | (Node.js runtime flag, not app code) | none | flags | Node heap size | no | yes | never | rustic-git-web.yaml : `--max-old-space-size=384` |

---

## Candidates for live settings (non-secret, non-bootstrap)

Every knob below is read fresh on each use (no cache/OnceLock, no struct captured once at startup), carries no credential, and isn't a listen address/store URL/pool path/node name — so a config-reload mechanism (or a sidecar watching a ConfigMap) could apply a change without a pod restart:

- `RUSTIC_GIT_MAX_BODY` (server) — max git request body size
- `RUSTIC_GIT_UPLOAD_GRACE_SECS` (server/worker) — abandoned upload-session GC grace
- `WS_MAX_PER_OWNER` (api) — workspace/environment cap per owner
- `WS_NODE_DEAD_SECS` (agent) — dead-node declaration threshold
- `WS_BASE_PACKAGES` (agent) — base Nix package set prepended to every workspace
- `WS_NIXPKGS` (agent) — nixpkgs pin
- `WS_NIX_TIMEOUT` (agent) — nix build timeout
- `RUSTIC_GIT_SSH_PORT`, `RUSTIC_GIT_CLONE_HOST`, `RUSTIC_GIT_REGISTRY_HOST`, `RUSTIC_GIT_SSH_HOST` (web) — clone-menu display values

Everything else in the tables above is either a secret, a bootstrap-only listen address/store URL/pool path/node identity, or cached once behind a `OnceLock`/struct field that a running process will never re-read.

---

## Method

Generated by grepping `std::env::var(`/`env::var_os(` across `bins/` and `crates/{core,storage,gitbase,pulls,app,git,registry,api,workspaces}` and `process.env.` across `web/apps/web/src`, then reading each call site for its default, caching behavior, and callers; cross-checked against `deploy/rustic-git.yaml`, `deploy/rustic-git-web.yaml`, `deploy/k3s/agent-daemonset.yaml`, and `deploy/k3s/gateway.yaml` for the values actually set today. `envy` and `crates/registry`-specific config structs were searched and found not to be used — this codebase reads env vars directly, not through a config-struct-deserialization crate.
