# Building and pushing images from a workspace

Status: draft for review, 2026-09-09. One decision (§1, where the builder's kernel is) is left
to a spike named there; everything else is settled by this document.

## Why

A workspace is where the code is, and the code has a Dockerfile. Today `docker build` inside one
fails at the first step — there is no daemon, no buildkit, and the container has no capability
that could start either — so the loop is "push the branch, wait for CI, pull the image", which is
the loop a workspace exists to remove.

The second half is worse than missing: it is refused. `registry::auth::allow`
(`crates/registry/src/auth.rs`) grants a push only when the caller IS the image's owner. A person
is never a team, so no personal credential can push to `registry/{team}/{image}` — the directory
check that git-over-SSH already applies (`App::may_act(user, owner)`, membership through the
directory, cached 60 s) is never consulted by the registry. Every team member who has tried has
seen `DENIED: insufficient scope`, whether or not they are an admin of that team.

## Decisions taken

| Question | Answer |
| --- | --- |
| Where buildkit runs | In its own pod beside the workspace, `build-{ws}`, rendered and torn down by the workspace's controller with the workspace pod, on the same node. Never inside the workspace container: gvisor plus `drop: ALL` cannot host it, and the dev pod's privileged sidecar is a shape no tenant may have. |
| Why not a shared per-node buildkitd | A build cache is a side channel: every `RUN` layer of every tenant on the node would sit in one store, readable by the next tenant's cache hit. One builder per workspace keeps the cache inside the tenant boundary the volume already draws. |
| Why not buildx's `kubernetes` driver | It needs a kubeconfig and RBAC inside the workspace container. The workspace pod runs with `automountServiceAccountToken: false` on purpose; handing a tenant sandbox the power to create pods in its namespace reverses that. |
| How the workspace reaches it | A unix socket on a hostPath directory the agent owns, `{pool}/build/{ws}/buildkitd.sock`, mounted read-write into both pods. No network, no TLS, no port. buildx's `remote` driver speaks to it. |
| Rootless, unprivileged | `moby/buildkit:rootless`, `runAsUser: 1000`, `--oci-worker-no-process-sandbox`, no capabilities added, `privileged` never. Rootless buildkit needs `seccompProfile: Unconfined` and `appArmorProfile: Unconfined` on its own container; that is the whole exception, and it is on the builder pod only, never the workspace's. |
| Where the cache lives | `{pool}/homecache/{owner}/buildkit`, the LOCAL per-(owner, node) subvolume every tool cache already goes to. Not the workspace volume: a build cache is large, node-bound, and must not be snapshotted, pushed or cloned. It is shared by a person's workspaces on one node, which is the same boundary `~/.cache` already has. |
| Who pays | The builder pod runs in the owner's namespace, so its limits count against the `owner-quota` ResourceQuota and `/v1`'s quota check charges them (§4). A build is the workspace's cost, not free. |
| Registry credential | A per-person registry bearer token (`Jwt::mint_registry`, the type `/v2/token` already issues), projected by the api into the `user-key` Secret it already writes into every workspace pod, and served to docker by a credential helper. No derived copy of the SSH key, no long-lived password, nothing minted by the agent (which holds no JWT key). |
| Team push | `allow()` consults `may_act(who, owner)` when the caller is not the owner — the exact rule git-over-SSH has used since the fingerprint-identity change. Membership is the permission; a team admin role is not required to push. |
| Ceiling on the token | 24 h, re-minted by the api's resync beat every `KEYS_RESYNC_SECS` (300 s); a Secret volume updates in place so the pod reads the fresh token on the next docker call. A stolen token is bounded by a day and by that person's own images plus the teams they are in. |

## Design

### 1. The builder pod (`bins/agent/src/controller/workspace.rs`, `crates/workspaces/src/k8s.rs`)

`apply_workspace` renders a second pod, `build-{ws}`, whenever it renders the workspace pod, and
deletes it wherever it deletes the workspace pod (stop, delete, move). Same node (`placement`),
same namespace, same owner labels, an ownerReference to the Workspace so a lost delete is still
collected. It is NOT part of the workspace pod: a pod's runtimeClass is pod-wide, and the
workspace's is gvisor.

```
image:  moby/buildkit:rootless  (pinned by digest in ClusterSettings, a Mark::Boot field)
args:   --addr unix:///run/buildkit/buildkitd.sock --oci-worker-no-process-sandbox
        --root /cache/{ws}          # per workspace under the owner's shared cache subvolume (§8)
env:    BUILDKITD_FLAGS=--oci-worker-no-process-sandbox
securityContext (container):
        runAsUser: 1000, runAsGroup: 1000, allowPrivilegeEscalation: false,
        capabilities: {drop: [ALL]},
        seccompProfile: Unconfined, appArmorProfile: Unconfined   # rootless buildkit's own requirement
volumes:
        socket  hostPath {pool}/build/{ws}      DirectoryOrCreate  -> /run/buildkit   (both pods)
        cache   hostPath {pool}/homecache/{owner} subPath buildkit  -> /cache
resources: request 250m / 512Mi, limit = the workspace's own cpu/memory limit (§4)
```

`{pool}/build/{ws}` is created by the agent (`mkdir` + `chown 1000`) before either pod, exactly
as `ensure_shared_home` does, and swept by the janitor with the other per-workspace directories
when the workspace is gone. The socket is a file inside a directory the tenant can write, which
is why the directory is per workspace and not per owner: two workspaces must never share a
builder, or one's `docker buildx prune` empties the other's queue.

The workspace pod mounts the same directory at `/run/buildkit`. `login_env` gains
`BUILDKIT_HOST=unix:///run/buildkit/buildkitd.sock`, and the platform rc file runs, idempotently,
`docker buildx create --name kl --driver remote "$BUILDKIT_HOST" --use` — so `docker buildx build`
and `docker build` (aliased through buildx) just work, and `docker buildx bake` too. The workspace
image gains the `docker` CLI and the `buildx` plugin, no daemon.

**Readiness.** `Ready` on the workspace does not wait for the builder — a workspace is usable
without ever building — but the builder's phase is reported as a condition, `Builder=True/Ready`
or `False/{Pending,CrashLoop}`, so `kl status` and the web can say why `docker build` hangs.

**The kernel it runs on — the one open decision.** The builder pod is rendered WITHOUT a
runtimeClass (runc, host kernel) in this draft: a tenant's `RUN` steps execute in rootless
buildkit's own containers, unprivileged, uid 1000, no capabilities, user-namespaced — the
same isolation every rootless-docker host relies on, and weaker than gvisor. gVisor documents
running rootless buildkit inside it (native snapshotter, no process sandbox); if that works on
this region's `runsc`, the pod gets `runtimeClassName: gvisor` and this paragraph disappears. The
spike is one pod on `session-0` and an hour; it decides one field in the pod spec and nothing
else in this document.

### 2. The credential (`crates/workspaces/src/api/keys.rs`, `crates/workspaces/src/k8s.rs`, image)

The api's `user` role already writes the `user-key` Secret into each owner namespace and
re-projects it on every key change and on the resync beat. It gains one key,
`registry-token`: `Jwt::mint_registry(person, "*", 86_400)`. The beat re-mints it every pass, so
the token in the pod is never older than 300 s plus one pass, and a person whose access is
removed stops being able to push within a day at the outside and within one beat for a team push
(§3 checks membership live, the token only says who you are).

The workspace pod already mounts `user-key` read-only at `/etc/kloudlite/ssh` (`USER_KEY_PATH`),
so the token lands at `/etc/kloudlite/ssh/registry-token` with no new mount. `docker-credential-kl`, a POSIX shell script in the
workspace image, answers `get` for the registry host with `{"Username": "<owner>",
"Secret": "<token>"}` and `erase`/`store` with success and no action. `~/.docker/config.json`
is rendered once by the rc file: `{"credHelpers": {"<registry host>": "kl"}}`. Nothing is ever
written to `auths`, so nothing long-lived is on disk.

`docker login` is never needed and is refused with a message naming this document if someone
tries: the helper IS the login.

### 3. Team authorization (`crates/registry/src/auth.rs`)

`allow()` today:

```
who == owner            -> allowed
public && !write        -> allowed
else                    -> challenge / DENIED
```

becomes:

```
who == owner                          -> allowed
public && !write                      -> allowed
who is Some(u) && may_act(u, owner)   -> allowed          # NEW: team membership, read and write
else                                  -> challenge / DENIED
```

`may_act` is the `App` method git-over-SSH resolves identity with: own handle, or team membership
through the directory, cached 60 s, and refused on `Source::Unavailable` — a directory outage is
a DENIED with a reason, never an allow. It is called only on the miss path, so an owner's own
pushes and every public pull cost what they cost today. The Bearer token's `scope` claim is not
consulted for this: `"*"` means "the person", and the image-level decision is made here, live.

This is the whole of "push to the team's registry". It also fixes `docker pull` of a private team
image by a member, which was refused the same way.

### 4. Quota (`crates/workspaces/src/api/mod.rs`, `crd::default_quota`)

`workspace_cost` charges the builder's limits beside the workspace's — the builder is part of
what a running workspace can consume, and the `owner-quota` ResourceQuota will refuse the second
pod otherwise, which reads as "workspace stuck Creating" instead of a 409 with a sentence. With
the builder's limit equal to the workspace's (4 vCPU / 8 GiB), a workspace charges 8 vCPU / 16 GiB.
`default_quota` is recomputed by the rule already written above it: person 5 × 8 + 2 × 4 × 2 =
**56 vCPU**, 5 × 16 + 32 = **112 GiB**; team 20 × 8 + 64 = **224**, 20 × 16 + 128 = **448**. The
test `the_default_cpu_and_memory_cover_the_counts_they_promise` fails until the table is updated,
which is the point of it. The live `default-user` / `default-team` objects are patched to match.

The builder's request stays small (250m / 512Mi): an idle buildkitd is idle, and nodes pack on
requests. Only the ceiling moves.

### 5. Networking

The builder pod is selected by the namespace's existing policies: `default-deny`,
`allow-dns`, `allow-internet-egress` (the registry host and every public base image are outside
RFC 1918). It has no ingress and needs none; the socket is a file. An attached environment's
services are NOT reachable from the builder — a build that needs them is a build that should
run against a pushed image — so `attach-{ws}` keeps selecting the workspace pod only.

### 6. Snapshots, clone, restore, move

Nothing here is in the volume. A push or clone carries no cache; a restore or a move to another
node starts with a cold builder there. `spec.state` gains nothing. The socket directory is
per-workspace and per-node and is recreated by the controller wherever the workspace lands.

### 7. Probe (`bins/slo`)

| id | suite | sli | target |
| --- | --- | --- | --- |
| `ws.build.p95` | hourly | `docker buildx build` of a two-line Dockerfile in the probe workspace, pushed to the probe owner's own image, and its manifest readable back through `/v2` | `p95(120_000)` |
| `registry.team.push` | hourly | a team member's personal credential pushes to the team's image, and a non-member's is DENIED | `avail(99.9)` |

The hourly already creates a team with the probe user in it and a second owner outside it, so
`registry.team.push` is an assertion on what teardown already stands up. Both ids follow the
rule this branch's predecessor learned: they are read by result from the step log, and a `skip`
is a hole, never a pass.

### 8. Failure modes

| Failure | Behaviour |
| --- | --- |
| Builder pod cannot schedule (ResourceQuota) | `Builder=False/Pending` on the workspace; `docker build` reports the socket absent; the workspace itself is unaffected |
| buildkitd crashes | Kubernetes restarts it; the socket path is stable so buildx reconnects on the next command |
| Token expired in a long-running pod | Impossible while the api runs: the beat re-mints every 300 s against a 24 h TTL. If the api is down for a day, pushes fail with the registry's challenge, and resume when it returns |
| Directory unavailable during a team push | DENIED, as every `may_act` refusal on `Source::Unavailable` is; never an allow |
| Person removed from the team | Next push is DENIED within the 60 s membership cache; the token itself keeps working for that person's own images |
| Workspace moved to another node | Builder re-rendered there; cache cold; nothing lost that was promised |
| Two workspaces of one owner on one node | Separate sockets, shared cache directory — buildkit locks its root, so the second builder must use `--root /cache/{ws}`; the spec's `--root` is therefore per workspace under the shared subvolume |

## Out of scope

`docker run` inside a workspace (a daemon, and a second sandbox question). Build cache
replication between nodes. Registry-side build triggers. A per-team registry quota — images are
already per-owner and counted by the registry's own GC, which this changes nothing about.

## Files

`crates/workspaces/src/k8s.rs` (builder pod, socket mount, `login_env`, credential file),
`bins/agent/src/controller/workspace.rs` (render/tear down with the pod, `Builder` condition,
socket directory), `crates/workspaces/src/api/keys.rs` (`registry-token` in `user-key`),
`crates/registry/src/auth.rs` (`may_act` on the miss path), `crates/workspaces/src/api/mod.rs` +
`crates/workspaces/src/crd/mod.rs` (quota), the workspace image (`docker`, `buildx`,
`docker-credential-kl`, rc file), `bins/slo` + `crates/workspaces/src/slo/catalogue.rs` +
`deploy/slo.md` + `web/apps/web/src/lib/fixtures/superadmin.ts` (two ids),
`deploy/k3s/crds.yaml` (nothing new: the condition is a condition), `CLAUDE.md` (one paragraph).
