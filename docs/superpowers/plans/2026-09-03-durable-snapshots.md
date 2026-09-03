# Durable Snapshots Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A pushed snapshot (commit) outlives the workspace or environment it came from; deleting the parent removes only its live worktree and automatic cuts; commits die only by explicit delete.

**Architecture:** Commit records become children of the Volume, transients stay children of the parent. A finalizer on every Workspace/Environment drops the worktree and, when the Volume holds a commit, removes the parent's ownerReference so the Volume detaches instead of being garbage-collected. A byte sweep on every node deletes `snap/<name>` subvolumes whose record is gone, so an explicit commit delete reclaims disk everywhere. `/v1`'s snapshot/volume deletes move from the legacy registry upstream to the CRDs, and the web's archived-volume listing (already there for environments) covers workspaces too.

**Tech Stack:** Rust (kube-rs finalizers, JSON patch), btrfs via `Engine`, Next.js web (`bun test`).

**Spec:** `docs/superpowers/specs/2026-09-03-durable-snapshots-design.md`

## Global Constraints

- A commit is a `Snapshot` with `spec.transient == false` and phase `Ready`; everything else is a transient and belongs to its parent.
- Deleting a parent never deletes a commit or its bytes. It deletes the parent's worktree subvolume, the parent's transient records, and the parent's ownerReference on the Volume; the Volume stays iff it holds a commit.
- Commit records carry a controller ownerReference to the **Volume**; the migration baseline, sync, stop and clone cuts carry one to the **parent**.
- Bytes of a record that no longer exists are deleted on every node that holds them by the pull beat's sweep; the sweep is keep-biased (a Snapshot list error deletes nothing) and never deletes a subvolume whose record exists.
- `DELETE /v1/volumes/{name}/snapshots/{id}`: 404 unless the caller owns the Volume; 409 `"this snapshot is the base of a running worktree"` when any live parent's `status.head` names it; else delete the record, and delete the Volume when it was the last commit of a detached Volume. `DELETE /v1/volumes/{name}`: 409 `"the volume still has a workspace or environment"` when a parent exists; else delete the Volume.
- A detached Volume keeps its pin, replicas and retention rules unchanged; nothing runs on it.
- The agent's Volume patch for ownerReferences is metadata-only and must be allowed by `deploy/k3s/agent-admission.yaml` and `agent-rbac.yaml` (whose header table IS the role); no spec write is added.
- Comments say why, never what; keep every `// ponytail:` marker; commit subjects imperative sentence case; no tool attribution anywhere in commit messages.
- Gates for every Rust task: `cargo test -p rustic-git-agent-bin -p rustic-git-workspaces -- --test-threads=1; echo exit=$?` (unpiped) and `cargo clippy --workspace --all-targets --locked -- -D warnings`; web tasks: `cd web && bun run lint && bunx tsc --noEmit -p apps/web/tsconfig.json && bun test`.

---

### Task 1: Commit records belong to the Volume, baselines to the parent

**Files:**
- Modify: `crates/workspaces/src/api.rs` — `create_commit` (~1900-1930, the `owner_references` assignment)
- Modify: `bins/agent/src/controller/workspace.rs` — `migrate_and_seed_baseline` (~608-640)
- Test: `crates/workspaces/tests/api_commit_model.rs`, `bins/agent/src/controller/volume.rs` tests (the baseline test) / `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Consumes: `crd::Volume` uid available where `create_commit` runs (it reads the Volume already? — if not, one `Api<crd::Volume>::get` by name; the parent's `status.volumeRef` names it).
- Produces: push commit `metadata.ownerReferences == [Volume controller ref]`; baseline `metadata.ownerReferences == [parent controller ref]`.

- [ ] **Step 1: Failing tests**

`api_commit_model.rs` (extend the existing `push_creates_a_working_snapshot_with_worktree_and_parent`, and add):

```rust
#[tokio::test]
async fn a_push_commit_is_owned_by_the_volume_not_the_workspace() {
    // fixture: placed workspace ws-1 whose status.volumeRef is "ws-1", plus GET /volumes/ws-1 returning uid "vol-uid-1"
    let (app, rec) = ...;
    let r = post(&app, "/v1/workspaces/ws-1/push", json!({"message": "m"})).await;
    assert_eq!(r.status(), 202);
    let body = rec.sent("POST", "/apis/rustic-git.io/v1alpha1/snapshots").pop().unwrap();
    let o = &body["metadata"]["ownerReferences"][0];
    assert_eq!(o["kind"], "Volume");
    assert_eq!(o["name"], "ws-1");
    assert_eq!(o["uid"], "vol-uid-1");
    assert_eq!(o["controller"], true);
}
```

Agent baseline test (`the_migration_baseline_is_owned_by_its_volume` in `controller/volume.rs` tests, or wherever it lives — grep it): rename to `..._is_owned_by_its_parent` and assert `kind == "Workspace"` / the parent's name and uid.

- [ ] **Step 2: Run, expect failure.**

- [ ] **Step 3: Implement** — `create_commit` gets the Volume (`Api::<crd::Volume>::all(c).get(volume)`; the name IS the volume id) and sets `snap.metadata.owner_references = Some(vec![vol.controller_owner_ref(&()).expect("a live Volume has a uid")])`. Remove the parent ref there. In `migrate_and_seed_baseline`, the ownerReference becomes the parent's (`owner_ref_of_kind(parent)`) — thread the parent (or its ref) in from both callers (`workspace.rs:823`, `environment.rs:246`) instead of `vol`. Keep the comment explaining WHY the two differ (rule 2 of the spec).

- [ ] **Step 4: Gates.**

- [ ] **Step 5: Commit** — `git commit -am "Own pushed commits by their Volume and baselines by their parent"`

---

### Task 2: A parent's finalizer drops its worktree and detaches a Volume that holds commits

**Files:**
- Modify: `bins/agent/src/controller/workspace.rs` — `reconcile_workspace` finalizer wiring (~550-575: today only a shared clone gets `WORKTREE_FINALIZER`), `cleanup_workspace_worktree` (~575-600)
- Modify: `bins/agent/src/controller/environment.rs` — the same finalizer for environments
- Modify: `bins/agent/src/controller/volume.rs` — new `pub(crate) async fn detach_volume(ctx, volume: &str, parent_uid: &str) -> Result<bool, kube::Error>` (JSON patch removing this parent's entry from `metadata.ownerReferences`, `test` on the current list first) beside `take_volume`/`release_volume`
- Modify: `deploy/k3s/agent-admission.yaml`, `deploy/k3s/agent-rbac.yaml` (allow the agent to patch `metadata.ownerReferences` on Volumes it already may patch labels/finalizers on; table + rule + why)
- Test: `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Consumes: `engine.drop_worktree(volume, ws)` (`engine/commit.rs:149`), `crd::WORKTREE_FINALIZER`, `Api<crd::Snapshot>` list by `spec.volume`/`spec.worktree`.
- Produces: `detach_volume`; every Workspace/Environment carries `WORKTREE_FINALIZER` from its first reconcile; `cleanup_parent(kind, obj, ctx)` shared by both kinds.

- [ ] **Step 1: Failing tests** (`bins/agent/tests/reconcile.rs`, recorder-based like the existing finalizer tests — grep `WORKTREE_FINALIZER` there):

```rust
/// Deleting a workspace that pushed keeps its commits: the worktree goes, the transients go, and the
/// Volume is detached (this parent's ownerReference removed) rather than left for GC.
#[tokio::test]
async fn deleting_a_workspace_with_a_commit_detaches_its_volume() {
    // fixture: ws-1 with deletionTimestamp + WORKTREE_FINALIZER, status.volumeRef ws-1, uid "ws-uid";
    // Volume ws-1 with ownerReferences [ws-uid]; snapshots: one Ready commit (transient=false) and one
    // Ready `sync-ws-1-aaaa` transient owned by ws-1.
    ...
    let patches = rec.sent("PATCH", "/apis/rustic-git.io/v1alpha1/volumes/ws-1");
    assert!(patches.iter().any(|p| p.to_string().contains("ownerReferences")), "the Volume was detached");
    assert!(rec.calls().iter().any(|c| c.starts_with("DELETE /apis/rustic-git.io/v1alpha1/snapshots/sync-ws-1-aaaa")));
    assert!(!rec.calls().iter().any(|c| c.contains("DELETE /apis/rustic-git.io/v1alpha1/volumes/")), "the Volume itself is never deleted here");
    assert!(!rec.calls().iter().any(|c| c.contains("snapshots/ws-1-") && c.starts_with("DELETE")), "the commit stays");
}

#[tokio::test]
async fn deleting_a_workspace_without_a_commit_leaves_the_volume_to_gc() {
    // same fixture, no commit — only transients
    ...
    assert!(rec.sent("PATCH", "/apis/rustic-git.io/v1alpha1/volumes/ws-1").is_empty(), "ownerReference kept so GC deletes the Volume");
}

#[tokio::test]
async fn detach_volume_is_a_guarded_patch_on_the_current_owner_list() {
    // detach_volume issues [test ownerReferences == current, replace ownerReferences == current minus this uid]
}
```

Add the environment twins (`deleting_an_environment_with_a_commit_detaches_its_volume`).

- [ ] **Step 2: Run, expect failure.**

- [ ] **Step 3: Implement**
  - Every parent (not just shared clones) gets `WORKTREE_FINALIZER` on first reconcile; the cleanup arm becomes `cleanup_parent`: `engine.drop_worktree(volume, id)` (already idempotent), delete Snapshot CRs with `spec.worktree == id && spec.transient` (list once, delete each; 404 fine), then list `Snapshot`s for the Volume and if any is `Ready && !transient` → `detach_volume(ctx, &volume, &parent_uid)`; otherwise leave the ownerReference. An owned (non-clone) workspace whose worktree name equals the volume id: `drop_worktree` must drop the live subvolume only, never `snap/` — confirm in `engine/commit.rs` and add a unit test if it is not already covered.
  - `detach_volume`: JSON patch `[{op: test, path: /metadata/ownerReferences, value: <current>}, {op: replace, path: /metadata/ownerReferences, value: <current without uid>}]`; `Ok(false)` on 409/422 (someone else changed it; the finalizer requeues).
  - Admission: extend the agent's allowed metadata writes on `volumes` to `ownerReferences` (only removal — if CEL can express "new list ⊆ old list", do that; else document the ceiling with a `# ponytail:`). RBAC table row + rule why.
  - The finalizer's environment side mirrors this with `e.name_any()` as the worktree.

- [ ] **Step 4: Gates.** Also `kubectl apply --dry-run=server -f deploy/k3s/agent-admission.yaml` against the k3s kubeconfig if reachable (`.local/k3s.yaml`), report the result.

- [ ] **Step 5: Commit** — `git commit -am "Detach a Volume that holds commits when its parent is deleted"`

---

### Task 3: Bytes follow records — the byte sweep on every node

**Files:**
- Modify: `bins/agent/src/peer.rs` — beside `retire_pass` (~1144) add `sweep_orphan_snap_bytes(ctx, beat)`; call it from the pull beat after `retire_pass`
- Modify: `crates/workspaces/src/engine/commit.rs` — `pub fn list_snaps(&self, volume) -> Vec<String>` if none exists (read `pool.snap_dir(volume)`)
- Test: `bins/agent/src/peer.rs` `mod tests` (tempdir pool like the `orphan_voldirs`/`retire_pass` tests)

**Interfaces:**
- Consumes: `engine.drop_commit(volume, name)` (`peer.rs:687` already uses it), `beat.volumes`, `Api<crd::Snapshot>` list.
- Produces: `sweep_orphan_snap_bytes`.

- [ ] **Step 1: Failing tests**

```rust
#[tokio::test]
async fn the_byte_sweep_drops_a_snap_whose_record_is_gone_and_keeps_the_rest() {
    // pool: vol/v1/snap/{v1-aaaa, v1-bbbb}; Snapshot list returns only v1-aaaa (Ready commit)
    // after the sweep: v1-aaaa present, v1-bbbb gone; recorder shows no DELETE of any CR
}
#[tokio::test]
async fn the_byte_sweep_deletes_nothing_when_the_snapshot_list_fails() { ... }
#[tokio::test]
async fn the_byte_sweep_never_touches_a_working_or_pending_record() {
    // a snap dir whose record exists but is Working (mid-cut) is kept
}
```

- [ ] **Step 2: Run, expect failure.**

- [ ] **Step 3: Implement** — one Snapshot LIST per beat (the beat may already hold it — reuse if `Beat` carries snapshots; else list once). For each Volume this node holds (`interesting_volumes` or `beat.volumes` filtered by local voldir presence), for each `snap/<name>` directory: keep if a record named `<name>` exists (any phase); else `drop_commit`. Skip a volume whose voldir is mid-migration (`{id}.owner` marker semantics — read `orphan_voldirs`'s neighbours). Keep-biased on list error. Mark the ceiling: `// ponytail: one full snap listing per volume per beat; index by name if volumes grow past thousands of commits`.

- [ ] **Step 4: Gates.**

- [ ] **Step 5: Commit** — `git commit -am "Delete snapshot bytes whose record is gone on every node"`

---

### Task 4: `/v1` snapshot and volume deletes move to the CRDs; listing counts commits

**Files:**
- Modify: `crates/workspaces/src/api.rs` — `delete_snapshot` (~2256), `delete_volume` (~2239), `list_volumes` (~2095-2140), `live_parents` (~2056: also collect each parent's `status.head` and `status.volumeRef`)
- Modify: `crates/workspaces/src/model.rs` — `VolumeSummary` gains `commits: u64`, `last_push_at: Option<String>`
- Modify: `deploy/k3s/api-rbac.yaml` — `snapshots: delete`, `volumes: delete` for the api ServiceAccount (table + rule + why)
- Test: `crates/workspaces/tests/api_commit_model.rs` (or `api_volumes.rs`)

**Interfaces:**
- Consumes: `volume_owner` (ownership check), `crd::Snapshot` list by volume.
- Produces: the two routes' new semantics; `VolumeSummary.commits/last_push_at`.

- [ ] **Step 1: Failing tests**

```rust
#[tokio::test] async fn deleting_a_commit_that_is_a_running_worktrees_head_is_a_409() { ... }
#[tokio::test] async fn deleting_a_commit_removes_its_record() { ... /* DELETE snapshots/<id> recorded; Volume untouched (another commit remains) */ }
#[tokio::test] async fn deleting_the_last_commit_of_a_detached_volume_deletes_the_volume() { ... /* DELETE snapshots/<id> then DELETE volumes/<name> */ }
#[tokio::test] async fn deleting_the_last_commit_of_an_attached_volume_keeps_the_volume() { ... }
#[tokio::test] async fn deleting_a_volume_with_a_parent_is_a_409() { ... }
#[tokio::test] async fn deleting_a_detached_volume_deletes_it() { ... }
#[tokio::test] async fn a_foreign_volume_is_not_found_on_delete() { ... }
#[tokio::test] async fn volume_rows_carry_the_commit_count_and_last_push() { ... }
```

- [ ] **Step 2: Run, expect failure** (the two deletes currently answer 503 `registry upstream not configured` under the test harness, or go to the mock upstream).

- [ ] **Step 3: Implement** — replace the `upstream(&s)?` calls: `delete_snapshot` lists Snapshots for the Volume, refuses when `live_parents`' heads name the id, deletes the CR, then re-lists: if no Ready non-transient remains and no parent names the Volume → delete the Volume CR. `delete_volume`: parent exists → 409; else delete. `list_volumes` rows: `commits` = Ready non-transient count, `last_push_at` = newest such `readyAt`. Remove the now-unused `upstream.delete_*` (and `registry` trait fns if nothing else calls them; if the `Upstream` type is still used by other routes leave the type). Update the doc comments.

- [ ] **Step 4: Gates.**

- [ ] **Step 5: Commit** — `git commit -am "Delete snapshots and volumes through the CRDs and count commits per volume"`

---

### Task 5: The janitor deletes a Volume with no parent and no commit

**Files:**
- Modify: `bins/agent/src/peer.rs` (beside the orphan sweeps in `retire_pass`) or `bins/agent/src/janitor.rs` — pick the beat that already lists Volumes and Snapshots (the pull beat does; the janitor does not) → `retire_pass`
- Test: `bins/agent/src/peer.rs` tests

- [ ] **Step 1: Failing test** — a Volume with no ownerReferences, no parent naming it in `beat.parents`, and no Ready commit, older than one `WS_REPLICA_SECS`, is deleted by the node that OWNS it (its `spec.nodeName`); a Volume with a commit is kept; a Volume younger than one beat is kept (a restore may be mid-create).

- [ ] **Step 2: Run, expect failure.**

- [ ] **Step 3: Implement** — owner-node only (one deleter, no race), keep-biased on any list error.

- [ ] **Step 4: Gates.**

- [ ] **Step 5: Commit** — `git commit -am "Delete a detached Volume once its last commit is gone"`

---

### Task 6: Web — Snapshots for workspaces, counts, delete actions, delete-dialog copy

**Files:**
- Modify: `web/apps/web/src/lib/api.ts` — `ApiVolumeSummary` gains `commits: number; last_push_at: string | null`; `deleteVolumeSnapshot`/`deleteVolume` already exist
- Create: `web/apps/web/src/app/(shell)/[owner]/(org)/workspaces/snapshots/page.tsx` — the workspace twin of `environments/page.tsx`'s archived section (or extend the environments page into one "Snapshots" page with a kind filter — follow whichever the shell nav makes natural; read `shell-nav.tsx`)
- Modify: `web/apps/web/src/components/app/env-snapshots.tsx` and the workspace snapshots page — use `commits` from the row instead of one history read per archived volume (delete that `ponytail:` loop)
- Modify: delete dialogs for workspace and environment (`workspace-list.tsx`, `env-actions.tsx`) — copy: "Your N pushed snapshots stay under Snapshots; unpushed changes are deleted." (N from the row's history count if cheap, else "Your pushed snapshots stay …")
- Test: `bun test` for any pure helper (e.g. the copy builder)

- [ ] Steps: failing test for the copy builder → implement → gates → `git commit -am "Show detached snapshots for workspaces and say what a delete keeps"`

---

### Task 7: Docs and the e2e

**Files:**
- Modify: `CLAUDE.md` — the "Volume is a CHILD… deleting the parent is the whole delete" sentence becomes: deleting the parent deletes its worktree and transients; a Volume with commits detaches and outlives it; commits die only by explicit delete; the byte sweep reclaims disk everywhere.
- Modify: `deploy/k3s/README.md` — "Release: durable snapshots": apply `agent-admission.yaml` + `agent-rbac.yaml` + `api-rbac.yaml` BEFORE the agent/api roll (the finalizer's ownerReference patch and the api's deletes need the grants).
- Modify: `tests/ws_e2e.sh` — the restore block: delete the source (already), assert the Volume still exists with `commits ≥ 1` in `/v1/volumes`, restore, assert bytes + frozen state, then delete the restored workspace, delete the commit(s) via `/v1/volumes/{name}/snapshots/{id}`, and assert the Volume CR disappears and `{pool}/vol/<id>` is gone on the node.

- [ ] Steps: edit → `bash -n tests/ws_e2e.sh` → grep every name → `git commit -am "Document durable snapshots and assert them end to end"`

---

## Self-review

- Spec coverage: rule 1 (Task 2), rule 2 (Task 1), rule 3 (no change; Task 5 test asserts a detached Volume is kept when it has a commit), rule 4 (existing restore path — Task 7 e2e asserts re-attach), rule 5 (Tasks 3 + 4), rule 6 (Task 4), listing (Tasks 4 + 6), delete-dialog copy (Task 6), docs/e2e (Task 7), janitor safety net (Task 5), admission/RBAC (Tasks 2 + 4).
- Placeholders: test bodies in Tasks 2, 4 name the assertions and the recorded calls; fixture wiring is left to the file's existing helpers by design (the reviewer checks the assertions, not the fixture names).
- Type consistency: `detach_volume` (Task 2) used only there; `VolumeSummary.commits/last_push_at` (Task 4) consumed by Task 6 as `commits`/`last_push_at`; `sweep_orphan_snap_bytes` (Task 3) standalone; `WORKTREE_FINALIZER` reused.

## Revision 2026-09-03 (final vocabulary)

Applies on top of the tasks above (see the spec's revision section):

- **Task 1 (done):** stands — a push record is owned by the Volume.
- **Task 2/2b/2c/2d (done):** `cleanup_parent` keeps records where `!spec.transient` (a snapshot) and deletes the rest; the "pinned" predicate is replaced by `crd::Snapshot::is_snapshot()` in Task 4.
- **Task 4 (in flight):** push writes a snapshot (`transient: false`), `spec.pinned` is REMOVED from the CRD (regen), retention's push-pruning arm and `WS_SNAPSHOT_KEEP` are removed, the baseline becomes a sync point unless a head-bearing record is required (implementer explains), deletes/list per the brief's amendments, no pin/unpin routes.
- **Task 5:** unchanged (safety sweep: a Volume with no parent and no snapshot, older than one beat, is deleted by its owner node).
- **Task 6:** wording — "Snapshots" lists pushes; sync points never shown; delete-dialog copy: "Your N snapshots stay under Snapshots; unpushed changes are deleted."
- **Task 7:** docs use only workspace / environment / push / snapshot / restore / clone / delete; "sync point" appears in CLAUDE.md as the internal name of transients.
- **Task 8 (new): sync on definition change, both kinds.** `bins/agent/src/sync.rs`: `sync_one` cuts when the generation moved OR `live.state != newest_transient.spec.state` (the newest Ready transient of that worktree; absent ⇒ cut). Environments are already in `live_worktrees` — assert with a test. Tests: unchanged bytes + changed packages ⇒ a cut whose `state` carries the new packages; unchanged both ⇒ no cut; environment services change ⇒ cut. Update the sync.rs module doc and the CLAUDE.md sentence on when the sync beat cuts.
- **Redundancy removed by this revision:** `spec.pinned`; `WS_SNAPSHOT_KEEP` and the push-pruning arm; the legacy registry `upstream.delete_volume/delete_snapshot` and their `registry` trait fns (Task 4); the word "commit" in user-facing docs and web copy (internal fn names such as `create_commit`, `clone_commit`, `commit_model_history_rows` may stay; the final review lists any that mislead).
