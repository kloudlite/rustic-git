# Building and pushing images from a workspace

Status: draft for review, 2026-09-09, revised the same day after review, and again after the
spike in §1 settled the kernel question. The builder is an Environment the platform owns, not a
new kind; it is never listed by `/v1` or the web; it has no user-facing snapshots; and it runs
only while a build is running.

## Why

A workspace is where the code is, and the code has a Dockerfile. Today `docker build` inside one
fails at the first step — no daemon, no buildkit, and a container with no capability that could
start either — so the loop is "push the branch, wait for CI, pull the image", which is the loop a
workspace exists to remove.

The second half is refused rather than missing. `registry::auth::allow`
(`crates/registry/src/auth.rs`) grants a push only when the caller IS the image's owner. A person
is never a team, so no personal credential can push to `registry/{team}/{image}` — the directory
check git-over-SSH already applies (`App::may_act(user, owner)`) is never consulted by the
registry. Every team member who has tried has seen `DENIED: insufficient scope`.

## Decisions taken

| Question | Answer |
| --- | --- |
| What the builder is | An `Environment`, `bld-{owner}` — one per owner, a person or a team — with a single service `buildkit` (`moby/buildkit:rootless`) whose mount folder `cache` IS the build cache. Placement, claim, replicas, stop-and-move on decommission, quota: all inherited, none written. `spec.system: "builder"` marks it. |
| Why not a new kind | A third worktree parent touches every place the reconciler matches on "workspace or environment" — the 24 settle sites already audited once. An Environment already is a snapshotted, replicated, placed volume with services and `desiredState`. |
| Why not inside the workspace, or one daemon per node | The workspace container is gvisor plus `drop: ALL`; nothing can start a daemon in it. A shared per-node buildkitd has ONE `--root`: every tenant's layers in one content store, cross-tenant cache hits, and no way to switch cache per build. One builder per owner keeps the cache inside the boundary the volume already draws. |
| Hidden | Never listed: `GET /v1/environments` omits `system` environments, every other environment route answers 404 for one as if it did not exist, and the web never sees it. It is not counted in the owner's `environments` quota. |
| No snapshots | No push, restore, clone or history: those routes are among the 404s. The volume still carries the platform's own SYNC POINTS — a stop cuts one, peers pull it, retention keeps one — because that is what replication is. Nothing non-transient is ever cut, `status.head` never advances, and nothing about it appears anywhere a person reads. |
| On demand | `desiredState: Stopped` at rest. A region-wide gate the workspace's buildx connects through starts it on the first connection, waits for `Ready`, splices the bytes, and stops it after `BUILDER_IDLE_SECS` with no connection open. cpu and memory are charged only while Running (`quota::usage` already counts only live parents). |
| Who writes its spec | `bins/api`, and only it, as for every CR: created with the owner's first workspace, `desiredState` flipped by the gate THROUGH the api. The gate never writes a CR. |
| Registry credential | A per-person registry bearer token (`Jwt::mint_registry`, the type `/v2/token` issues), projected by the api into the `user-key` Secret it already writes into every workspace pod, served to docker by a credential helper. No derived copy of the SSH key, nothing long-lived, nothing minted by the agent. |
| Team push | `allow()` consults `may_act(who, owner)` when the caller is not the owner — the rule git-over-SSH has used since the fingerprint-identity change. Membership is the permission. |

## Design

### 1. The builder environment

**Identity.** `bld-{slug}`, where `slug` is the owner: a person's handle for their own
workspaces, the team's slug for a team's. Namespace `env-bld-{slug}` through `crd::env_namespace`
as for every environment, so nothing about namespaces, labels, quotas or policies is special. One
service:

```
name:      buildkit
image:     moby/buildkit:v0.18.2            (pinned by digest in ClusterSettings, Mark::Boot)
command:   buildkitd --addr tcp://0.0.0.0:1234 --oci-worker-snapshotter=native --root /cache
ports:     [1234]
mounts:    [{path: /cache, folder: cache}]
resources: request 250m / 512Mi, limit PodResources::default() (4 vCPU / 8 GiB)   # §4
```

`spec.system: "builder"` is a new optional field on `EnvironmentSpec`, written only by the api
and only here. `model::Service` gains an optional `resources` (the env unit stays the default):
a builder wants the ceiling a workspace has, not a service's.

**Created** by the api's `user` role, server-side apply, the first time it writes a Workspace for
that owner (the same call that projects `OwnerKeys`), with `desiredState: Stopped` and a volume of
`BUILDER_CACHE_GB` (50, a `Quota`-charged `diskGb`). Idempotent: a second workspace changes
nothing. **Deleted** by the resync beat that prunes `OwnerKeys` and `wt-` namespaces, one beat
after the owner has no Workspace left — the cache goes with the last workspace, which is when
nothing can use it. A person's builder and a team's are distinct objects.

**Rendered** by the existing environment controller with no builder-specific branch except the
per-service `resources`. Stop, start, claim, move, replicate, `Replicated`, the dead-node sweep:
unchanged code.

**The kernel it runs under — decided by the spike (2026-09-09).** The builder runs under the
region's `runtime_class` (gvisor) like every environment service. Rootless buildkit cannot run
under gvisor: rootlesskit needs `newuidmap` capabilities that `drop: ALL` forbids, and `buildkitd`
refuses a non-root user without a user namespace. What works — six attempts, the last building a
`RUN` step and caching the second build in 1 s — is `buildkitd` as ROOT INSIDE THE SANDBOX with
`drop: ALL` and an explicit add-back: the container runtime's default set (`CHOWN`,
`DAC_OVERRIDE`, `FOWNER`, `FSETID`, `SETUID`, `SETGID`, `SETPCAP`, `SETFCAP`, `MKNOD`,
`SYS_CHROOT`, `KILL`, `NET_BIND_SERVICE`, `NET_RAW`, `AUDIT_WRITE`) plus `SYS_ADMIN` for the
snapshotter's bind mounts; `seccompProfile: RuntimeDefault`, `allowPrivilegeEscalation: false`,
the plain `moby/buildkit` image, `--oci-worker-snapshotter=native`. Every one of those
capabilities is gvisor's, not the host kernel's: the isolation the builder relies on is exactly
the one every workspace already relies on. `hardened()` is not changed — a `system == "builder"`
environment's service is rendered with its own `builder_hardened()`, and the api is the only
writer of `system`. No `seccomp: Unconfined` exists anywhere, and there is no runc fallback.

### 2. The gate (`bins/gateway`, a second listener — or `bins/builder-gate` if the split is cleaner)

A region-wide Deployment in `kloudlite-system`, one replica, reached at
`builder-gate.kloudlite-system.svc:1234`. The workspace's `login_env` gains
`BUILDKIT_HOST=tcp://builder-gate.kloudlite-system.svc:1234`, and the platform rc file runs,
idempotently, `docker buildx create --name kl --driver remote "$BUILDKIT_HOST" --use`. `docker
build`, `docker buildx build` and `bake` need nothing else; the workspace image gains the `docker`
CLI and the buildx plugin, no daemon.

On a connection:

1. **Who.** A TCP connection carries no identity, so the gate reads the peer address and looks the
   pod up by IP in a reflector over `kloudlite.io/kind=workspace` pods (the same label selector
   the agent's own pod watch uses). The pod's owner and team labels name the builder:
   `bld-{team}` for a team workspace, `bld-{owner}` otherwise. No pod at that address is a closed
   connection.
2. **Start.** `POST /v1/internal/builders/{slug}/start` on the api, authenticated by the shared
   secret the peer listeners already use (`WS_PEER_SECRET`'s pattern, its own variable). The api
   writes `desiredState: Running` and answers 202; the gate polls the environment (through the
   same internal route, GET) until `Ready=True`, `BUILDER_START_SECS` (120) at most, then dials
   `buildkit.env-bld-{slug}.svc:1234` and splices both directions. A start that runs out of time
   closes the connection; buildx reports the endpoint unreachable, and the environment's own
   condition says why (`ServicesNotReady`, a ResourceQuota refusal, `Placed=False`).
3. **Stop.** The gate counts open connections per builder. When a builder's count has been zero
   for `BUILDER_IDLE_SECS` (600, a central live setting), `POST …/stop`; the api writes
   `desiredState: Stopped`; the environment controller cuts the stop sync point and tears the
   pod down; peers pull. A connection arriving during the stop starts it again on the next pass —
   the same rule a workspace stopped mid-`kl start` follows.
4. **Restart of the gate.** Counts are in memory. On start the gate lists Running builders (the
   internal GET) and begins an idle timer for each: the worst case of a gate restart is one idle
   period of compute, never a builder left running.

Two NetworkPolicies, both written by the reconcilers that own the namespaces: `allow-builder-gate`
in every owner namespace (egress from workspace pods to the gate's pods, the shape `allow-dns` has),
and `allow-builder-gate` in `env-bld-{slug}` (ingress to `buildkit` from the gate's pods, the shape
`allow-gateway-ssh` has in workspace namespaces). The builder is otherwise as closed as any
environment: `default-deny`, `allow-dns`, `allow-internet-egress` — the registry host and every
public base image are outside RFC 1918. An attached environment's services are not reachable
from a build, on purpose; that is what a pushed image is for.

### 3. The credential (`crates/workspaces/src/api/keys.rs`, `crates/workspaces/src/k8s.rs`, image)

The api's `user` role already writes the `user-key` Secret into each owner namespace, but today
only re-projects it on a key change — the resync beat (`keys::run_beat`) re-projects `OwnerKeys`
only, and never touched `user-key`. This task adds a resync pass for `user-key` too, so the
Secret (and the `registry-token` key below) is rewritten every beat as well as on every key
change. It gains one key, `registry-token`: `Jwt::mint_registry(person, "*", 86_400)`, re-minted
every pass, so the token in the pod is never older than one beat plus one pass and a stolen one
is bounded by a day and by what §4 lets that person do.

The workspace pod already mounts `user-key` read-only at `/etc/kloudlite/ssh` (`USER_KEY_PATH`),
so the token lands at `/etc/kloudlite/ssh/registry-token` with no new mount. `docker-credential-kl`,
a POSIX shell script in the workspace image, answers `get` for the registry host with
`{"Username": "<owner>", "Secret": "<token>"}` and `store`/`erase` with success and no action.
`~/.docker/config.json` is rendered once by the rc file: `{"credHelpers": {"<registry host>":
"kl"}}`. Nothing is ever written to `auths`. `docker login` is not needed and the rc file's
`docker` wrapper refuses it with a sentence naming this document: the helper IS the login.

The builder pod itself pulls base images anonymously or from the team's own registry with the
same helper: the `buildkit` service's pod mounts the same Secret, and the daemon reads
`DOCKER_CONFIG`. A private base image of another team is DENIED, as §4 says it must be.

### 4. Team authorization (`crates/registry/src/auth.rs`)

```
who == owner                          -> allowed
public && !write                      -> allowed
who is Some(u) && may_act(u, owner)   -> allowed      # NEW: team membership, read and write
else                                  -> challenge / DENIED
```

`may_act` is the `App` method git-over-SSH resolves identity with: own handle, or team membership
through the directory, cached 60 s, refused on `Source::Unavailable` — a directory outage is a
DENIED, never an allow. It runs only on the miss path, so an owner's own pushes and every public
pull cost what they cost today. The token's `scope` claim is not consulted: `"*"` means "this
person", and the image-level decision is made here, live, against the directory.

This is the whole of "push to the team's registry". It also fixes a member's `docker pull` of a
private team image, refused the same way today.

### 5. Hidden, and not counted (`crates/workspaces/src/api/environments.rs`, `quota.rs`, web)

- `GET /v1/environments` filters `spec.system.is_some()` out. Every other environment route —
  get, start, stop, delete, attach, intercept, clone, restore, push, history, snapshots — answers
  404 for a `system` environment, so it is indistinguishable from an environment that does not
  exist. The two internal routes in §2 are the only way to touch one, and they take a slug, not an
  environment id.
- `quota::usage` skips `system` environments for the `environments` count and charges their
  `diskGb` and, while live, their cpu and memory; `environment_cost` is not what created it (the
  api did, at a fixed size), so the count dimension never sees it. The derived defaults in
  `crd::default_quota` gain the builder's ceiling once per owner: person **40 vCPU / 80 GiB**,
  team **148 / 296**; the derivation test enforces it, and the live `default-*` objects are patched.
- The web reads nothing new. `kl` gains `kl builder status` (the internal GET, through the api,
  for the caller's own builder) so "why is my build hanging" has an answer; nothing else.

### 6. Snapshots, clone, restore, move

Inherited and, for people, invisible. A stop cuts `stop-{env}-{gen}` and peers pull it; a node
death or a decommission moves the builder as it moves any environment; retention keeps one sync
point. No `Snapshot` with `transient: false` is ever written for a builder — push is a 404 — so
history is empty by construction and `status.head` is never advanced. A moved builder starts on
the up-to-date node with the cache as of its last stop, which is the last build.

### 7. Probe (`bins/slo`)

| id | suite | sli | target |
| --- | --- | --- | --- |
| `ws.build.p95` | hourly | `docker buildx build` of a two-line Dockerfile in the probe workspace, pushed to the probe owner's own image, its manifest readable back through `/v2`; the builder was Stopped before the step, so this measures the on-demand start too | `p95(180_000)` |
| `registry.team.push` | hourly | a team member's personal credential pushes to the team's image, and a non-member's is DENIED | `avail(99.9)` |
| `builder.hidden` | hourly | the probe owner's builder is absent from `GET /v1/environments` and its id answers 404 on get, start, push and snapshots | `avail(99.9)` |

The idle stop is unit-tested in the gate with a paused clock and deliberately NOT probed:
`BUILDER_IDLE_SECS` is 600, and an hourly step cannot wait it out. Both build ids are read by
result from the step log, and a `skip` is a hole, never a pass.

### 8. Failure modes

| Failure | Behaviour |
| --- | --- |
| Builder cannot start (ResourceQuota, no capacity, `Placed=False`) | gate closes the connection after `BUILDER_START_SECS`; `kl builder status` shows the environment's own condition; the workspace is unaffected |
| buildkitd crashes mid-build | the StatefulSet restarts it; buildx reports the build failed; the next `docker build` reconnects through the gate |
| Gate restarts | every Running builder gets a fresh idle timer; worst case one idle period of compute |
| Token expired in a long-lived pod | impossible while the api runs (re-minted every beat against a 24 h TTL); with the api down for a day, pushes fail with the registry's challenge and resume when it returns |
| Directory unavailable during a team push | DENIED, as every `may_act` refusal on `Source::Unavailable` is |
| Person removed from the team | next push DENIED within the 60 s membership cache |
| Builder's node dies mid-build | the build fails; the builder is interrupted like any environment and the cache as of its last stop is what the next start gets, on an up-to-date node |
| Two builds from one team at once | one daemon, concurrent builds — buildkit's ordinary behaviour; `ponytail:` one builder per owner is the ceiling, sharded builders or registry-exported cache is the upgrade path |

## Out of scope

`docker run` inside a workspace. Registry-exported build cache (`--cache-to type=registry`) for
cross-node or cross-region sharing — the opt-in for a team that outgrows one builder. Sharded
builders. A per-team registry quota.

## Files

`crates/workspaces/src/crd/mod.rs` (`EnvironmentSpec.system`), `crates/workspaces/src/model.rs`
(`Service.resources`), `crates/workspaces/src/k8s.rs` (per-service resources, `login_env`,
`allow-builder-gate` policies), `crates/workspaces/src/api/environments.rs` (hidden routes, the
two internal routes, builder create), `crates/workspaces/src/api/keys.rs` (`registry-token`,
builder prune), `crates/workspaces/src/quota.rs` + `crd::default_quota`,
`crates/registry/src/auth.rs` (`may_act`), the gate (`bins/gateway` or `bins/builder-gate`),
`bins/kl` (`kl builder status`), the workspace image (`docker`, buildx, `docker-credential-kl`, rc
file), `bins/slo` + catalogue + `deploy/slo.md` + web fixtures (three ids), `deploy/kloudlite.yaml`
+ `deploy/k3s/*.yaml` (the gate, its RBAC, `crds.yaml`), `CLAUDE.md` (one paragraph).
