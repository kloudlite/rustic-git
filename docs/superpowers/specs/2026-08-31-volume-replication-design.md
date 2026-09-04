# Replicating a volume to standby nodes — design

Date: 2026-08-31. Status: draft for review.

## Problem

A volume's data exists on exactly one node. `status.nodeName` is written once by the node that
claims it and is never cleared, so where a volume first landed is where it stays. When that node is
under load, the owner's workspaces cannot start anywhere else — there is no copy to start from.

This design pre-stages copies. It does NOT move anything: placement, draining and eviction are out
of scope (see "Not in scope").

## What already supports this

Three things were built for it, which is why this is additive rather than structural:

- `status.compatibleNodes` is documented as "every node that holds this object's volume", with the
  explicit note that "nothing in it may assume there is only one (replication across nodes is a
  later design)".
- `claim::may_claim` is already `compatible.is_empty() || compatible.iter().any(|n| n == me)` — a
  node listed in `compatibleNodes` may claim the object when it is unplaced. Adding a replica node
  to that list makes it eligible with NO change to the claim path.
- `claim::decide`'s 409 retry comment already anticipates a peer that "may have only widened
  `compatibleNodes`" — concurrent widening is a modelled case, not a new one.

`claim::source_nodes` follows from the same field: a clone must land where its source's disk is, so
replicating a source widens where its clones may be placed too.

## Design

### Replica count

`WS_REPLICA_COUNT` on the agent, an integer N meaning TOTAL copies including the primary. `1` is
today's behaviour and the default — no replication, no peer traffic, nothing new runs.

Actual copies are `min(N, pooled nodes in the region)`. A 2-node cluster with `N=3` keeps 2 and
reaches 3 when a third node joins; the beat re-evaluates every pass, so growth is picked up without
an operator step.

### Choosing the targets

Rendezvous (highest-random-weight) hashing of the volume id over the region's pooled nodes: each
candidate node scores `hash(volume_id, node_name)`, the top `N-1` excluding the owner are the
targets.

Chosen over "least loaded" deliberately:

- **No coordinator.** Every agent computes the same set from the same inputs, so a sender and a
  receiver agree without asking anyone.
- **Minimal reshuffling on growth.** Adding a node moves only ~1/N of volumes, where a modulo scheme
  would move nearly all of them — which matters because every move is a full send.
- **Stable.** The same volume keeps the same targets across restarts, so an interrupted replication
  resumes against the same peer.

### Transport

The agent gains a peer listener. It has none today; it only calls out to the server tier.

- `POST /peer/v1/replicate/{owner}/{id}` — request body is a btrfs send stream, response is the
  received snapshot's name. The receiver pipes the body to `btrfs receive` into `{pool}/repl/{id}/`
  — NOT `{pool}/recv/`: the janitor's recv sweep keeps only snapshots named in some volume's
  lineage (`janitor_sweep_recv`), and replica snapshots are in no lineage, so parking them in
  `recv/` would have every replica silently deleted one age-floor after it lands. `repl/` gets
  its own keep-biased sweep instead.
- `GET /peer/v1/snapshots/{owner}/{id}` — what the receiver already holds, so the sender can pick a
  parent for `-p`.
- Auth is a shared secret in a header, the same shape the server tier's peer listener uses. The
  agent already holds a token for `WS_REGISTRY_URL`; this is a second, separate secret so a leaked
  registry token cannot drive node-to-node writes.
- A headless Service gives per-pod DNS. A NetworkPolicy admits only other agent pods.

### What is sent

A read-only snapshot of `live`, exactly as `push` already stages one — replication does NOT require
the volume to be stopped, because a RO snapshot is a consistent point regardless of what the
workspace is doing. (Stopping is a constraint on MOVING, which is out of scope here.)

Incremental whenever possible: the sender asks the receiver what it holds and sends `-p` against the
newest common snapshot. A receiver holding nothing takes a full send.

### Affinity groups

Clones share extents with their source on the owning node, which is why they are co-located. Sending
them independently destroys that: each clone arrives as a FULL copy, so a five-clone group costs 5x
on the target.

So the unit of replication is the group, sent in order: the common ancestor first, then each clone
against it. The mechanism is `-c` (clone source), not `-p`: `-p` is same-volume incremental ("send
what changed since my own previous snapshot"), while `-c` names another subvolume whose extents the
receiver already holds, so the stream references them instead of carrying them. A clone's send uses
`-p` against its own previous replica snapshot when one exists, plus `-c` naming the ancestor's
replica snapshot when the receiver holds it. If a `-c` send fails (btrfs is picky about
relatedness), fall back to the full send — correctness over sharing, keep-biased. The group is the
set of volumes on this node whose `cloneOf` chain reaches the same root, sent ancestor-first.

### When it runs

A background beat on the agent, in the shape of the existing home-push beat: every
`WS_REPLICA_SECS` (default 300), for each volume this node owns, replicate if the source's btrfs
generation has moved past `{voldir}/.replicated-gen-{node}` for that target. Idempotent, retried on
the next beat, and never in the path of a user-visible verb.

### Sender-side snapshot retention

`-p` needs the previous snapshot on BOTH sides. The sender therefore keeps its last replicated
snapshot per volume in its own `{pool}/repl/{id}/`, deleting an old one only once every target
holds a newer one. Without this the next beat has no local parent and every send is full.

### Recording it

After a successful receive, the RECEIVER adds itself to the PARENT's (`Workspace`/`Environment`)
`status.compatibleNodes` with a guarded write, retrying on 409 by re-reading — the same pattern
`claim::decide` uses. The parent, not the Volume: `claim::may_claim` reads the parent's list, and
the Volume CRD carries no such field. The receiver is the only party that knows the data actually
landed. Home volumes are excluded from v1 outright — their parent is the OwnerBinding, which has
no `compatibleNodes`, and blank 3 already leaves their replication undecided.

Removal is the reverse: a node that drops a replica removes itself.

## Failure modes

| Failure | Behaviour |
|---|---|
| Peer unreachable | Beat logs and returns; retried next beat. Never blocks the owning node's own work. |
| Send dies mid-stream | `btrfs receive` leaves a partial subvolume. The receiver deletes it before returning an error — a partial must never be advertised, so `compatibleNodes` is written only after a clean receive. |
| Receiver out of space | Receive fails, partial cleaned, node not added. The volume keeps however many replicas it has. |
| Source volume deleted mid-send | Send fails; the receiver's partial is swept. Replica dirs for a volume with no CRD are swept by the janitor, same keep-biased shape as its siblings. |
| Node added or removed | Rendezvous set changes; new targets replicate on the next beat, and nodes no longer selected drop their replica and remove themselves from `compatibleNodes`. |
| Two agents replicate the same volume | Cannot arise: only the OWNING node sends, and ownership is `status.nodeName`. |
| Receiver already holds a newer snapshot | Sender picks it as the `-p` parent; a no-op send when generations match. |
| `compatibleNodes` write races the claim path | Guarded write with re-read on 409 — already the claim path's own mechanism. |

## Not in scope

Moving or evicting a running workload. Clearing `status.nodeName`. Load-aware placement. Draining a
node. Cross-region replication (each region keeps its own copy today and nothing syncs them; that is
unchanged). Replacing the registry as the durable tier — this is availability, not durability.

## Tests

- `crates/workspaces` units: rendezvous selection is deterministic, excludes the owner, returns
  `min(N-1, others)`, and moves ~1/N of volumes when a node is added.
- Affinity-group ordering: a group is sent ancestor-first and each clone names the ancestor as `-p`.
- `bins/agent/tests/reconcile.rs`: a successful receive adds the node to `compatibleNodes`; a failed
  one does not; a 409 re-reads and retries.
- Peer listener: auth rejects a missing or wrong secret; a partial receive leaves no subvolume and
  no status write.
- Generation gate: an unchanged volume sends nothing on the next beat.
- Measured on the cluster: with `N=2`, a volume replicates to the peer, `compatibleNodes` shows both
  nodes, and the replica's subvolume matches the source's contents.

## Blanks

1. **Space.** N=2 doubles pool usage. The pool is 1% full of 1 TiB today, so this is not urgent, but
   nothing here bounds total replica space or sheds replicas under pressure.
2. **Where the peer secret comes from.** The `kloudlite-git-jwt` Secret is the existing pattern; this
   needs its own key and a decision about rotation.
3. **Interaction with the home volume.** A home is per-owner-per-node by design and every workspace
   of that owner on that node mounts it. Replicating a home to a node that holds none of the owner's
   workspaces may be pointless, or may be exactly the point. Undecided.
4. **`N` is cluster-wide.** Per-owner or per-tier replica counts are not addressed.
