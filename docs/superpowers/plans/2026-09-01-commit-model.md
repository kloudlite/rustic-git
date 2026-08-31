# Commit Model Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Volumes become repositories of immutable btrfs-snapshot commits, pulled by every replica
node, with worktrees runnable on any node holding their checkout commit — replacing the
object-store lineage subsystem end to end.

**Architecture:** Two reworked CRDs (`Snapshot` as a commit record, new `VolumeReplica` as
per-node truth); a pull beat on the agent replacing the push beat; commit/checkout/retention in
the engine; claim-predicate and unclaim changes in `claim.rs`; then deletion of the blob/lineage
path and a hand-run cutover.

**Tech Stack:** Rust (kube-rs, axum, tokio), btrfs, k3s.

**Spec:** `docs/superpowers/specs/2026-08-31-replica-only-snapshots-design.md` — read it first.
Its "Decision and what it costs" section is binding context: cross-region and restore-from-blob
are deliberately gone, and `N>=2` is a durability requirement.

## Global Constraints

- **One fact, one object, one writer.** `Snapshot` immutable after Ready; `Workspace.status.head`
  written only by the node running it; `VolumeReplica` written only by `spec.node` — the sole
  exceptions are DELETION of a dead node's replica rows and CLEARING a dead node's claims, both
  gated on NotReady > `WS_NODE_DEAD_SECS` (default 600), both guarded writes.
- **CR first, subvolume second.** A commit's CR is created before its btrfs snapshot is cut, so a
  retry finds the CR and continues. Names are `{volume}-{8-hex uuid}`; ORDER comes only from
  `spec.parent`.
- **Commit subvolumes live at `{pool}/vol/{volume}/snap/{name}`, worktrees at
  `{pool}/vol/{volume}/live/{workspace}`.** Nothing new is written into `recv/`, `stage/` or
  `img/`, and no `.lineage` file is ever created by new code.
- **Keep-biased everywhere:** a CR-list error keeps every local subvolume; a failed pull leaves no
  partial (delete before returning error, the receive-side rule already proven in peer.rs); a
  reap needs positive NotReady evidence, never a lookup failure.
- **Inert until enabled:** everything new is gated on `WS_COMMIT_MODEL=1` until Task 7's cutover,
  so every intermediate commit deploys safely. The old push path keeps working until Task 8
  deletes it.
- The btrfs sync-before-snapshot, the per-volume flock (`ws_lock`), and `valid_segment` on every
  path segment from a request all stay exactly as they are.
- Comments explain WHY at the density of `bins/server/src/router/route.rs`. Commit subjects
  imperative sentence case, no attribution.
- `CARGO_INCREMENTAL=0`, FOREGROUND, long timeout, never backgrounded — three prior tasks stalled
  by backgrounding cargo.
- `CARGO_INCREMENTAL=0 cargo test --workspace --locked` and
  `CARGO_INCREMENTAL=0 cargo clippy --workspace --all-targets --locked -- -D warnings` green per
  task. `tests/routing.rs` flakes under parallel load — re-run alone.

---

### Task 1: The two CRDs

**Files:**
- Modify: `crates/workspaces/src/crd.rs` — add `Snapshot` + `SnapshotStatus`, `VolumeReplica` +
  `VolumeReplicaStatus`; add `replicas: u32` (default 2) to `VolumeSpec`; add `head: Option<String>`
  and `durable: Option<String>` to `WorkspaceStatus` and `EnvironmentStatus`
- Regenerate: `CRD_REGEN=1 cargo test -p rustic-git-workspaces --test crd_yaml` (deploy/k3s/crds.yaml is generated — never hand-edit)
- Test: `mod tests` in crd.rs, and the crd_yaml snapshot test

**Interfaces (produces):**
```rust
pub struct SnapshotSpec { pub volume: String, pub owner: String,
    #[serde(default)] pub parent: String,          // "" = root
    #[serde(default, skip_serializing_if = "Option::is_none")] pub message: Option<String>,
    #[serde(default)] pub pinned: bool }
pub struct SnapshotStatus { pub phase: Phase,      // Working until the subvolume is cut
    #[serde(default, skip_serializing_if = "Option::is_none")] pub size_bytes: Option<u64> }
pub struct VolumeReplicaSpec { pub volume: String, pub node: String }
pub struct VolumeReplicaStatus { pub phase: String, // "Synced" | "Syncing"
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")] pub branches: BTreeMap<String,String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub last_sync_at: Option<String> }
pub fn snapshot_name(volume: &str) -> String       // {volume}-{8 hex}
pub fn replica_name(volume: &str, node: &str) -> String  // {volume}.{node}
```
Selectable fields: `Snapshot` `.spec.volume`; `VolumeReplica` `.spec.node` and `.status.phase`
(strings only — the constraint crd.rs:287 already records). Labels stamped as views, same doctrine
as every other kind. Both cluster-scoped, `Snapshot` ownerReferenced to its Volume,
`VolumeReplica` too.

- [ ] Step 1: failing tests — `snapshot_name` shape/uniqueness; `replica_name` determinism; a
  serde round-trip per new type; `VolumeSpec.replicas` defaults to 2 on old JSON.
- [ ] Step 2: watch them fail. Step 3: implement. Step 4: regen crds.yaml and eyeball the diff for
  the two new kinds and the selectable fields. Step 5: full suite + clippy. Step 6:
  `git commit -m "Add the Snapshot and VolumeReplica kinds"`.

---

### Task 2: Commit and checkout in the engine

**Files:**
- Create: `crates/workspaces/src/engine/commit.rs`; register in `engine/mod.rs`
- Modify: `crates/workspaces/src/engine/pool.rs` — `snap_dir(volume)`, `worktree(volume, ws)`
- Test: new `mod tests` with a loopback pool where root+btrfs exist (mirror `engine_ops.rs`'s
  fixture gating), pure-path tests everywhere else

**Interfaces (produces):**
```rust
impl Engine {
  /// Cut commit `name` from worktree `ws` of `volume`. CR already exists (Working).
  pub fn commit_worktree(&self, volume: &str, ws: &str, name: &str) -> Result<(), EngErr>;
  /// Create worktree `ws` from commit `name` (RW snapshot), or empty when `name` is None (bootstrap).
  pub fn checkout(&self, volume: &str, name: Option<&str>, ws: &str) -> Result<(), EngErr>;
  /// Commits present on this pool for `volume` (dir listing of snap_dir).
  pub fn local_commits(&self, volume: &str) -> Result<Vec<String>, EngErr>;
  /// Delete a commit subvolume (retention / reconcile).
  pub fn drop_commit(&self, volume: &str, name: &str) -> Result<(), EngErr>;
}
```
`commit_worktree` = sync, `btrfs subvolume snapshot -r live/{ws} snap/{name}`, under `ws_lock`.
`checkout` = `btrfs subvolume snapshot snap/{name} live/{ws}` (or `subvolume create` for None);
refuses an existing worktree. All paths through `Pool` helpers, never formatted inline.

- [ ] Step 1: failing tests — commit+checkout round-trip preserves content; checkout of a missing
  commit errors without creating anything; bootstrap checkout makes an empty worktree; drop_commit
  of a commit a checkout came from leaves the worktree readable (the CoW independence the design
  rests on — pin it in a test).
- [ ] Steps 2-5 as usual. Step 6: `git commit -m "Add commit and checkout to the engine"`.

---

### Task 3: The pull beat and VolumeReplica writer

**Files:**
- Modify: `bins/agent/src/peer.rs` — add `GET /peer/v1/commit/{volume}/{name}?parent={p}`
  streaming `btrfs send` (auth + `valid_segment` + per-volume mutex, same discipline as the
  existing receive handler); add `pull_beat`; keep the push half untouched (Task 8 deletes it)
- Modify: `bins/agent/src/controller.rs` — spawn `pull_beat` beside `spawn_replicate`, gated on
  `WS_COMMIT_MODEL=1`
- Modify: `deploy/k3s/agent-rbac.yaml` — snapshots + volumereplicas verbs, table and rules together
- Test: `bins/agent/tests/peer.rs` + reconcile-style tests with the fake API

**One `pull_beat` pass, per volume where `targets(volume, ready_nodes, replicas)` names me OR I
run one of its worktrees:**
1. List `Snapshot` CRs (`spec.volume` selector), phase Ready. List-error => keep everything, warn, next volume.
2. `missing = CRs - local_commits()`, ordered so parents precede children (reuse
   `replicate::order_groups` over `(name, parent)` — it is exactly this sort).
3. For each missing: pick a source from `VolumeReplica`s whose `branches` reach it (fallback: any
   Synced replica), `GET /peer/v1/commit/...` with `-p` = my newest ancestor of it; stream into
   `snap_dir`; failed receive deletes the partial.
4. Rewrite MY `VolumeReplica` (create-or-update, it is mine alone): `branches` from local commits
   joined with Workspace heads, `phase` = Synced iff nothing missing, `last_sync_at` = now.
5. Reap: any `VolumeReplica` whose node is NotReady past `WS_NODE_DEAD_SECS` — DELETE only,
   positive evidence only (a nodes-list error reaps nothing).

- [ ] Step 1: failing tests — the GET streams and refuses bad auth/segments; a pull with a
  CR-list error touches no subvolume; a clean pull sets Synced and writes branches; a dead-node
  row is reaped and a NotReady-but-young one is not; the beat is inert without `WS_COMMIT_MODEL`.
- [ ] Steps 2-5. Step 6: `git commit -m "Pull commits to every selected node and record replicas"`.

---

### Task 4: Placement on the checkout predicate, and unclaim

**Files:**
- Modify: `bins/agent/src/claim.rs` — `may_claim` gains the commit-model arm: I may claim an
  unplaced Workspace iff my `VolumeReplica` for its volume is Synced OR `branches[ws]` covers its
  `status.head` (spec "Where a workspace may run"); old arm kept behind the flag until Task 8
- Modify: `bins/agent/src/controller.rs` — on claim, materialize the worktree via
  `Engine::checkout(volume, head, ws)` when absent; write `Workspace.status.head` after every
  commit and checkout; add the unclaim sweep beside the reaper: clear `status.nodeName` (guarded)
  when its node is NotReady past `WS_NODE_DEAD_SECS`
- Test: reconcile tests with the fake API

- [ ] Step 1: failing tests — a Synced node claims; a Syncing node claims only when its branches
  cover the head; an unplaced workspace with NO replica anywhere is left unplaced (never started
  dataless — the spec's source_nodes rule generalized); a dead node's claim is cleared once and
  the survivor re-claims; a Ready node's claim is never cleared.
- [ ] Steps 2-5. Step 6: `git commit -m "Place worktrees where their commit lives and release dead claims"`.

---

### Task 5: Snapshot reconcile, retention, bootstrap

**Files:**
- Modify: `bins/agent/src/snapshot.rs` — a controller for `Snapshot` (kept name, new kind): on a
  Working snapshot whose volume's worktree runs HERE, `commit_worktree`, set Ready + sizeBytes,
  advance `Workspace.status.head`; on a deleted CR, `drop_commit` everywhere (each node reconciles
  its own pool)
- Retention, in the same reconcile on the worktree's node: keep newest `WS_SNAPSHOT_KEEP`
  (default 10) per branch, everything `pinned`, and NEVER the current `durable` floor; delete by
  deleting the CR — the per-node reconciles do the disk work
- Bootstrap: `resolve_volume` under the flag creates the empty worktree (`checkout(volume, None, ws)`)
  when the volume has no commits
- Test: reconcile tests; a retention test proving the durable floor survives the keep-window

- [ ] Steps 1-5 in the usual shape. Step 6: `git commit -m "Reconcile commits with retention that keeps the durable floor"`.

---

### Task 6: The verbs over the new model

**Files:**
- Modify: `crates/workspaces/src/api.rs` — `/push` creates a `Snapshot` CR (Working) and returns
  its name; `/restore` = clear+`checkout` at the named commit (moves `head` back — history
  branches, never rewrites); `/clone` = new Workspace whose `head` names the source's commit, no
  new Volume; `/v1/volumes/{name}/history` and `/refs` read `Snapshot` CRs + Workspace heads
  instead of the registry; `SnapshotRequest` creation stops (flag-gated)
- Test: `crates/workspaces/tests/api_user.rs` additions

- [ ] Steps 1-5. Step 6: `git commit -m "Serve push, restore, clone and history from the commit model"`.

---

### Task 7: Cutover — the flag becomes the default

**Files:** `bins/agent/src/lib.rs`, `deploy/k3s/agent-daemonset.yaml` (`WS_COMMIT_MODEL=1`),
`deploy/k3s/README.md` (cutover runbook)

Runbook order (documented, hand-run): apply crds.yaml → roll the agent with the flag → for every
existing volume, one `Snapshot` CR is minted from its current live (the migration commit) → verify
every volume shows a Synced `VolumeReplica` on N nodes → only then Task 8.
**Homes are volumes in this model** — each owner's home gets a Volume with one worktree per node;
this REPLACES the home-push beat's durability. The migration commit for homes comes from the same
path. If this proves wrong in review, stop and re-ledger: it is the one place this plan goes
beyond the spec, which still lists homes as excluded — the spec must be amended to match, since
deleting the push path (Task 8) otherwise leaves homes with NO durability at all.

- [ ] Steps: docs + defaults + the migration-commit helper (`kl`-invocable or a one-shot agent
  subcommand in the `main.rs` style of `squash`). Commit: `git commit -m "Default the commit model on and document the cutover"`.

---

### Task 8: Delete the object-store subsystem

**Files (delete/trim, table = the spec's "What is deleted" section):** `engine/blob.rs`,
`registry_client.rs`, `bins/server/src/vol_agent.rs` + its routes, `ops.rs` push/pull/squash/
lineage (~500 lines), `model.rs` LineageEntry+LayerKind, agent `blob_store()`/`WS_REGISTRY_URL`/
AZURE wiring/home-push beat, the old push half of peer.rs + `.replicated-gen-*` gates +
`sweep_stale_gates`, janitor sweeps for `recv/`/`stage/`/`img/`, the `SnapshotRequest` kind and
its reconcile, `/v2`-side nothing (registry images are NOT this subsystem — touch nothing under
`crates/registry`). RBAC + daemonset env cleaned, table and rules together.

- [ ] Step 1: grep-driven deletion, compiler as the guide; every existing test that pinned the old
  path is deleted WITH its subject, never weakened around it. Step 2: full suite + clippy.
  Step 3: `git commit -m "Delete the object-store lineage subsystem"`.

---

### Task 9: Pool cleanup runbook

`deploy/k3s/README.md`: after the cutover has verified Synced replicas everywhere — delete
`recv/`, `stage/`, `img/`, `.lineage`/`.pushed-gen`/`.replicated-gen-*` sidecars, old `repl/`
staging, and the Azure container's `layers/` blobs. Irreversible; gated on the Task 7
verification, exactly like the hostpath cutover was.

---

## Self-review

**Spec coverage.** CRDs incl. selectable-string constraint (T1); commit/checkout/CoW-independence
(T2); pull+heal+reap (T3); checkout-predicate scheduling + unclaim + never-dataless (T4);
CR-first idempotency, retention with the durable floor, bootstrap (T5); verbs + branching restore
(T6); flag cutover + migration commits (T7); the deletion table (T8); pool cleanup (T9).

**Known deviation, flagged loudly in T7:** homes join the model; the spec still excludes them.
Deleting the push path without this leaves homes with no durability — the spec must be amended,
or T8 must keep the home-push beat. This is the plan's largest open risk and is written where the
implementer and reviewer will both trip over it.

**Type consistency.** `snapshot_name`/`replica_name` (T1) used by T3/T5; `commit_worktree`/
`checkout`/`local_commits`/`drop_commit` (T2) used by T3/T4/T5; `order_groups` reused as the
parent-order sort (T3). `Phase` reuses the existing enum.

**Not covered on purpose.** Metrics (spec's "Not in scope" says they should land with this — add
counters opportunistically in T3/T5 where a warn already exists, no dedicated task). Cross-region.
Quota-per-repo billing question (spec's open item — T1 keeps quotaGb on Volume unchanged).

**Sizing note.** T3 and T4 are the judgment-heavy tasks (standard model); T1/T2/T5 are
well-specified (cheap-to-mid); T8 is mechanical deletion but large blast radius — standard model
plus the strongest review.
