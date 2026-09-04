# Direct container runtime control — design

**Status:** proposed
**Replaces:** `crates/workspaces/src/engine/compose.rs`, `bins/agent/src/container.rs`
**Related:** `docs/superpowers/audit-2026-08-25.md` (findings C1, H2, M1, M2)

## Problem

An environment's desired state lives in one place — the `Environment` document in Cosmos.
The agent then *materializes* part of that state as a `docker-compose.yml` on the pool disk
and hands it to the `docker compose` CLI, which parses it back. That file is a second,
derived copy of the truth, and it causes concrete failures:

- **Teardown depends on it.** `compose::down` (`compose.rs:78`) passes `-f {dir}/docker-compose.yml`,
  so an `EnvDelete` for an environment whose directory is missing or half-removed fails at
  `down` and exhausts its retries with local state stranded. Teardown of a resource should
  never depend on an artifact that setup happened to leave behind.
- **It is never cleaned up.** `cleanup_local` removes `vol/{id}` but not `{pool}/env/{id}`, so
  every deleted environment leaks its compose directory permanently. Two such orphans were
  found on the production VM on 2026-08-25.
- **It can drift.** Nothing re-renders the file when the `Environment` doc changes, so a file
  on disk and the document it came from can disagree with no detection.

The compose CLI is not earning this. The entire rendered surface is `image`, `command`,
`environment`, and bind `volumes` (`compose.rs:36-50`) — rendered from our own `Service`
struct (`model.rs:202`). No `depends_on`, healthchecks, `profiles`, `extends`, variable
interpolation, declared networks, or restart policies are used. We serialize our own model to
YAML so that a subprocess can deserialize it and make the same daemon API calls we could make
directly.

## Decision

Talk to the container daemon directly with [`bollard`](https://docs.rs/bollard), and make
**container labels the only source of truth on the node**. Nothing about an environment's
runtime shape is written to the pool disk.

### Which runtime

Target **Podman** (rootful) over its Docker-compatible socket, with `crun` as the OCI runtime.
Docker remains supported by the same code path, because the API is the same — the runtime is a
socket path and a version pin, not a fork in the logic.

Podman is chosen over Docker for weight (no long-running daemon, no BuildKit/compose/swarm we
do not use) and over containerd for features. containerd would mean owning CNI configuration
and running a DNS resolver ourselves; the whole point of leaving compose is to *reduce* what we
hand-roll, and per-environment networking with service-name resolution is the one thing compose
genuinely provided.

Everything this design needs is packaged on the agent host (Ubuntu 24.04): `podman` 4.9.3,
`netavark` 1.4 (bridge networking), `aardvark-dns` 1.4 (service-name DNS — the piece that makes
`mongodb://db:27017` resolve between services), `crun` 1.14.

Practical notes for the implementation:

- Rootful Podman: the agent already runs as root on the btrfs host, and rootful keeps
  Docker-equivalent semantics for bind mounts onto btrfs subvolumes.
- The API socket is a systemd unit (`podman.socket` → `/run/podman/podman.sock`) and must be
  enabled; it is not on by default. Make the socket path configurable
  (`KLOUDLITE_GIT_CONTAINER_SOCKET`) and default to Podman's, so a Docker host still works by
  pointing it at `/var/run/docker.sock`.
- Podman's Docker-compat API is close but not identical. Pin the negotiated version and fail
  startup with a clear message rather than at first job. Where an endpoint differs, prefer the
  libpod endpoint over emulating Docker.
- `crun` is set per-container at create time and is independent of the manager choice — it is
  worth adopting under Docker too.

**Isolation is a separate decision from weight.** Multi-tenant user-supplied images share one
kernel on a shared VM, so a container escape reaches every tenant on that node. If that becomes
a requirement, the answer is a stronger OCI runtime — gVisor (`runsc`) or Kata — dropped into
the same per-container runtime slot as `crun`, not a different manager. Out of scope here;
recorded so the choice stays open.

The `Environment` document remains the single authority. The daemon holds a labelled
projection of it. Reconciliation compares the two. There is no third copy.

### Labels replace the file

Every container the agent creates carries:

| Label | Value | Purpose |
|---|---|---|
| `kloudlite-git.owner` | `{owner}` | Reclamation and audit |
| `kloudlite-git.kind` | `env` \| `ws` | Distinguishes the two container shapes |
| `kloudlite-git.id` | `env-{id}` \| `ws-{id}` | Groups an environment's containers |
| `kloudlite-git.service` | `{service name}` | Identifies one service within an environment |
| `kloudlite-git.spec` | `{sha256 of the rendered service spec}` | Drives recreate-vs-leave-alone |

This single change is what fixes the teardown class of bugs: **`down` and `delete` become a
label query, so they need no local state at all.** They are correct on a fresh agent, after a
pool wipe, on a node that has never seen the environment, and on a retry after a partial
failure. The `M1` bug does not get patched; it stops being expressible.

## The runtime module

`container.rs` and `compose.rs` collapse into one module, `bins/agent/src/runtime/`. The
comment at `container.rs:4` justifying the split ("an env is a multi-service compose project,
a workspace is one plain container") stops being true once neither goes through compose: a
workspace is an environment with one service and a fixed mount pair. One code path, one set
of labels, one reconcile loop.

```
runtime/
  mod.rs      // Docker connection, label constants, error mapping
  spec.rs     // Environment/Workspace -> Vec<ContainerSpec>; spec hashing
  reconcile.rs// up / down / delete / start / stop, all label-driven
  net.rs      // per-environment network create/ensure/remove
```

### Operations

**`up(env, live)` — reconcile, not create.**

1. Ensure the network `kloudlite-git-{env.id}` exists (idempotent; `net.rs`).
2. Build the desired `Vec<ContainerSpec>` from `env.services`, hashing each.
3. List existing containers by label `kloudlite-git.id={env.id}`.
4. For each desired service: if a container exists with a matching `kloudlite-git.spec`, ensure
   it is started; if it exists with a different hash, remove and recreate it; if absent,
   pull-if-needed, create, connect to the network, start.
5. Remove any labelled container whose service is no longer in the desired set.

Step 4 gives idempotent replay for free, which is what a duplicated job needs (audit H2):
running `up` twice is a no-op the second time rather than a "container already exists" error.

**`down(env_id)`** — list by label, stop each. No spec, no file, no document required; the id
alone is enough.

**`delete(env_id)`** — list by label, force-remove each, then remove the network. Then, and
only then, the caller removes the subvolume.

**`start`/`stop`/`is_running`** — same label query, replacing the
`docker inspect -f '{{.State.Running}}'` stdout parsing at `container.rs:77` with a typed
`ContainerState`.

### Networking

Compose's one genuine contribution here is service-to-service DNS. Reproduce it explicitly:
one user-defined bridge network per environment, each container connected with a network
alias equal to its service name. That is what makes `mongodb://db:27017` resolve from another
service in the same environment, and it is a single `create_network` plus an `EndpointSettings`
with `aliases` at connect time.

The network is named `kloudlite-git-{env.id}` rather than `env-{id}` so it cannot collide with a
compose project network left over from the current implementation during migration.

**Always pass an explicit `--subnet`; never let the runtime auto-allocate.** Today compose's
default allocator hands each environment an entire `/16` — a live node was observed holding
`172.18/16`, `172.19/16`, `172.20/16` and `172.21/16` for four two-container environments. Since
`172.16.0.0/12` contains only 16 `/16`s, a node **exhausts the pool at roughly 16 environments**.
This is a live limit, not a hypothetical.

Allocate instead from a per-node block, locally:

- The node's block is a fixed configured range disjoint from the host VNet (the agent host's
  `eth0` is `10.0.0.4/24`, so `10.0.0.0/16` is out). A `/20` gives 256 environments.
- Each environment takes a `/26` (62 usable) from that block — sized for its services **plus**
  attached workspaces, not services alone.
- No central allocator, no coordination: node blocks do **not** need to be globally unique,
  because nothing routes between nodes (see below).
- Pin the runtime's `default_subnet_pools` to the node block as a backstop, and refuse to start
  if the block overlaps an existing host route.

Teardown removes the network along with the containers. Networks leak today exactly as the
compose directories do — deleted test environments were found still holding
`env-loop-clone-src/dst` networks.

### Cross-machine: deliberately none

There is no cross-region and no cross-team connectivity, by product decision. Combined with the
placement rules (a team's environments on one node, a user's workspaces on one node), the only
edge that would ever cross a machine is *a workspace reaching its own team's environment*.

That edge is closed by **placement, not by networking**: a workspace that attaches to an
environment is scheduled onto **that environment's node**, and then simply joins the
environment's network with a `network connect`. Placement follows the attachment; the
workspace's node comes from the environment's binding rather than the owner's.

Consequences, all of which must be enforced rather than assumed:

- **A workspace attaches to at most one environment.** Two attachments could name environments
  on two nodes, which is the only case that would force an overlay back into the design. Enforce
  at create time.
- **The attachment lives in the workspace's spec**, so reconcile re-establishes it after a
  reboot, an environment recreate, or a workspace restart. Attachment held only as live daemon
  state silently disappears and the workspace comes back unable to resolve anything.
- **Attach is an authorization boundary.** Joining an environment's network grants reach to every
  service in it, so check team membership (`MembershipCheck`) at attach time, not just at
  environment creation.

No WireGuard, no VXLAN, no overlay, no route distribution, no fleet IPAM. If multi-team users
ever need a workspace attached to environments on two nodes, that is the point to revisit this —
and the scoped answer would be a per-team WireGuard network spanning only that team's nodes,
never a fleet mesh.

### Naming

Container names stay byte-identical to what compose produces today: `env-{id}-{service}-1`
for environment services, `ws-{id}` for workspaces. `tests/ws_e2e.sh` and every runbook
command (`sudo docker exec env-{id}-db-1 mongosh ...`) keep working unchanged. Names remain a
human affordance only — **all lookups go through labels**, never names, so a manually renamed
or externally created container cannot be mistaken for ours.

## YAML as an input format

If users author compose-style YAML, parse it at the API boundary in `crates/workspaces/src/api.rs`
into `Vec<Service>` and store only the parsed model. The YAML is never persisted and never
reaches the agent.

Support exactly the keys the model has (`image`, `command`, `environment`, `volumes`, `ports`)
and **reject every other key with a 400 naming it**. Silently ignoring `depends_on` is worse
than refusing it: the user believes they expressed ordering they did not get. An explicit
refusal is a feature request; a silent drop is a support ticket.

`serde_yaml` is archived (RUSTSEC-2024-0320, audit finding 15); use `serde_yml` here.

## Two additions this unlocks

**Ports.** `Service` has no `ports` field, so nothing in an environment is reachable from
outside its network today. The runtime rewrite is the right moment: add
`ports: Vec<PortMap>` (`{container: u16, host: Option<u16>}`), map it to bollard's
`PortBindings`. Host port `None` means "publish on an ephemeral port", read back from the
container's `NetworkSettings` after start and surfaced on the `Environment` document so the
UI can link to it. Without this, environments can only be used from inside themselves.

**Label-driven janitor reclamation.** The janitor currently reclaims subvolumes but knows
nothing about containers, so a container whose `Environment` document is gone (a delete that
failed after the doc write, a Cosmos rollback) runs forever. With labels it becomes a
straightforward sweep: list all `kloudlite-git.kind` containers, ask the store which ids still
exist, force-remove the rest. This is only possible because labels make ownership queryable —
today there is no way to ask "which containers are ours?"

## Security: fold in the mount validation

The audit's one critical finding (C1) is in the code this design replaces: `Mount.folder` is
validated only for non-emptiness (`api.rs:536`) and then joined into a bind source
(`compose.rs:41`), and `Path::join` discards the base for an absolute component — so
`{"folder": "/"}` bind-mounts the host root into a user-chosen image on a root agent.

Do not carry that forward. In the new `spec.rs`, a bind source is constructed only from
`live.join("volumes").join(segment)` where `segment` has passed
`kloudlite_git_storage::store::valid_segment` (already rejects `.`, `..`, `/`, and anything
outside `[A-Za-z0-9._-]`). Validate at the API boundary as well, so a bad spec is refused at
write time rather than at materialization time. `Mount.path` must be absolute and contain no
`:`.

This is worth stating as a rule rather than a patch: **the runtime layer builds mount sources
from validated segments only; it never accepts a caller-supplied path.**

## Migration

Two environments are live on the production VM (`demo-env`, `mongo-test`) with
compose-created containers carrying compose's labels, not ours.

- `down`/`delete` match **either** `kloudlite-git.id={id}` **or**
  `com.docker.compose.project=env-{id}` for one release, so existing environments can still be
  torn down. Remove the compose branch afterwards; note it with a `// ponytail:` marker naming
  the removal trigger.
- `up` recreates containers under the new labels — an environment's data lives in the
  subvolume, not the container, so recreation is not data loss. It is a brief restart, which
  `EnvUp` already implies.
- `EnvDelete` additionally removes `{pool}/env/{id}` to clear the leaked directories, and
  tolerates its absence. After the deprecation window nothing writes that directory again.

## Testing

- **Unit:** spec hashing is stable across runs and changes when any field changes; mount
  source construction rejects `/`, `..`, `a/b`, empty, and `:`-bearing values (this is the C1
  regression test and it must exist before the code ships).
- **Integration, against a real daemon** (the agent test tier already requires one): `up` twice
  is a no-op the second time; `up` after a service's image changes recreates only that service;
  `down` succeeds with no local state present; `delete` on an already-deleted environment
  succeeds; two services in one environment resolve each other by service name.
- **e2e:** `tests/ws_e2e.sh` gains a service-to-service DNS assertion. The MongoDB clone
  fidelity check performed manually on 2026-08-25 becomes a scripted case: seed, clone, assert
  the clone's own database has the data, assert writes to the clone do not appear in the source.

## Not doing (and why)

- **`depends_on`, healthcheck gating, `profiles`, `extends`, interpolation** — none are used
  today. Start ordering, if it is ever wanted, should be an explicit scheduler concern, not
  inherited compose semantics.
- **A container-runtime abstraction trait.** One runtime, one implementation. Add the trait
  when a second runtime actually arrives.
- **Replacing `docker exec` as the workspace access path.** Out of scope; unchanged.
- **Rootless/Podman support.** Not requested.

## Risks

- ~250-300 lines of reconcile logic we own rather than borrow. Mitigated by how narrow the
  surface is: create, start, stop, remove, list-by-label, pull, network connect.
- bollard negotiates the daemon API version; pin a minimum and fail startup with a clear
  message rather than at first job.
- Losing `docker compose logs`/`ps` as debugging affordances. Both are daemon API calls we
  should expose in the product regardless; until then `docker ps --filter label=kloudlite-git.id=...`
  is the equivalent and works because the labels exist.
