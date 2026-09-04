# Cluster sync: pulling desired state instead of pushing it

**Status:** proposed
**Supersedes:** the "client-per-cluster" sketch in `2026-08-26-k3s-architecture-design.md`
**Related:** `2026-08-26-k3s-architecture-design.md` (CRDs, node controller)

## Problem

`/v1` writes CRDs. CRDs live in a workload cluster. `kloudlite-git-api` runs somewhere else — today
`kolomi-cluster`, whose API server has none of them. `kube::Client::try_default()` builds exactly
one client, so the API addresses exactly one cluster. With several clusters per region it addresses
the wrong one, or none.

## Rejected: the API holds a client per cluster

The obvious fix is a map of `cluster_id -> kube::Client`, built from credentials held centrally.
It is rejected for three reasons, in order of weight.

**Credential direction.** The API would hold a credential to every cluster. Compromise of one pod
is then compromise of the whole fleet. Every other design decision here has narrowed blast radius —
region-scoped agent tokens, per-namespace policies, a controller that watches only its own node —
and this would widen it further than any of them narrowed it.

**Inbound reachability.** Every cluster's API server would have to be reachable from the API tier.
That is a public endpoint or a tunnel per cluster, maintained forever, for clusters that otherwise
need no inbound path at all.

**Two problems it drags in.** A `workspace id -> cluster` index, because you cannot query every
cluster on every request; and cross-cluster fan-out for listing, with partial-failure semantics
when one cluster is down. Both are new distributed state in a design that just spent a migration
removing some.

## Decision

**Each cluster pulls its own desired state from the central API. Nothing pushes into a cluster.**

- **Central** owns desired state: which workspaces and environments exist, and which cluster each
  belongs to. This is user intent, and it is authoritative.
- **The cluster agent** reconciles that intent into local CRDs.
- **The node controller** converges CRDs into pods, volumes and policies. Unchanged.
- **Status flows back up** over the same connection, and central serves reads from it.

Credentials point one way only: a cluster holds a token for central; central holds none for any
cluster. A cluster needs no inbound reachability. Adding a cluster is issuing it a token.

This is the shape Flux, Argo CD and Rancher Fleet settled on, for the same reasons.

### This is not the job queue again

The deleted job queue carried *work items* — "run WsCreate", leased, retried, expiring. This
carries *desired state*: the full set of objects that should exist in this cluster. The controller
still converges it, the CRD is still the local source of truth, and none of the failure modes the
queue had (an unassigned job leased by the wrong agent, a stale report overwriting state, a sweep
that never marked the doc) are expressible. Pull is the distribution mechanism; reconciliation is
unchanged.

## What gets built

### 1. Cluster identity

There is no cluster concept in the model today — `region` is the only deployment-scope field, and
a region may hold several clusters. A `Cluster` record is needed: id, the region it sits in, its
own token, status. The existing `Region` keeps storage account and blob container, which are
genuinely regional (blobs are content-addressed, so clusters in a region can share a container).

`CommitRecord` gains a `cluster` field alongside `region`. Cheap now; expensive once volumes are
stamped, because existing records would need migrating.

### 2. Sync endpoint

`GET /v1/cluster/{id}/desired`, authorized by that cluster's token, returning every workspace and
environment assigned to it, with a generation.

**It returns an authoritative snapshot, not a changelog.** The agent's job is to make the cluster
match that snapshot. A changelog needs the agent never to miss an entry; a snapshot is
self-correcting — a missed sync is repaired by the next one.

### 3. Deletion, which is the dangerous part

If the agent infers deletion from absence, then "the workspace was deleted" and "the sync returned
a partial answer" look identical, and the second one deletes a user's data. Two guards, both
required:

- The snapshot is explicitly **complete or refused**. A partial answer is an error, never a short
  list. The agent reconciles deletions only against a response marked complete.
- Deletions carry **explicit tombstones** for a retention window, so the agent sees "this was
  deleted" rather than "this is missing".

Reconciling a delete still goes through the existing finalizer, so the subvolume is reclaimed
before the object disappears. Nothing about that ordering changes.

### 4. Status back-channel

The agent reports observed status upward — phase, conditions, pod state — and central stores it as
a **projection**. Reads and lists are served from the projection.

This is what removes the two problems the push design dragged in: no `id -> cluster` index, because
central already knows which cluster owns each object; and no fan-out listing, because the
projection is already local.

The projection is a view, never an authority. Where it disagrees with a cluster's CRD, the CRD is
right and the projection is stale. That is the same rule the k3s design already states for the
Cosmos projection, applied to the same data.

### 5. Placement

Pull does not answer which cluster a new workspace belongs to. Central still decides, at create
time, and writes the cluster onto the record; that cluster's agent then claims it on its next sync.

Node placement inside the cluster is unchanged — `OwnerBinding` and the existing `place()` still
pin an owner to a node. Cluster selection sits above it, and needs cluster health to choose well:
a cluster whose agent has not synced recently must not be handed new work.

## Costs, honestly

**Create is asynchronous.** The user sees "pending" until the target cluster's next sync. The poll
interval is the latency floor. A long-poll or watch narrows it; it never reaches zero.

**Central holds a projection**, which can be stale, and reads are served from it. Acceptable, and
already the stated rule — but it does mean a freshly-changed status is briefly not visible.

**One more moving part per cluster.** The agent gains a second responsibility beyond btrfs work.

## Migration

The plumbing exists. The agent already holds a region token and already talks to the server tier at
`/vol-agent/*`. This extends that surface rather than introducing a transport.

Ordering: `Cluster` record and the `cluster` field first (cheapest while few volumes are stamped),
then the sync endpoint, then the agent's sync loop, then the status back-channel, then cluster
selection. Each step is useful alone: the sync endpoint is inert until an agent calls it, and the
agent's loop is inert until placement assigns it work.

## Deliberately not here

- **Watch instead of poll.** Poll first; it is simpler and its failure mode is latency rather than
  a silently dead stream. Revisit when the latency floor is the complaint.
- **Central-initiated anything.** The moment central can push, it needs a credential to the
  cluster, and the whole reason for this design is gone.
- **Removing Cosmos.** It holds the cluster registry and the projection — precisely the things that
  cannot live inside any one cluster.
