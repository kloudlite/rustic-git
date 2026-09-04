# Phase 3: Workspaces Durability and Lease Correctness — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the workspaces-subsystem findings that lose data or run a job twice: durable lineage writes (H6a), a janitor that cannot sweep a live stage file (H6b), leases that survive a multi-minute `btrfs send` (H2) with engine replays that tolerate an existing subvolume, environment re-materialization that actually fires (H3), sweep-exhausted jobs that mark their doc `Error` (H7), and one job at a time per volume (M7).

**Architecture:** Six independent fixes across three layers, no new crates and no new modules.
1. *Pool layer* (`crates/workspaces/src/engine/pool.rs`): `set_lineage` becomes a tmp+rename write returning `Result`; every caller in `engine/ops.rs` propagates.
2. *Agent layer* (`bins/agent/src/lib.rs`): an age floor on the stage sweep, a per-job lease-renewal heartbeat spawned alongside each job, and `EnvUp` recovery keyed off registry history instead of the never-written `Environment.volume` pointer.
3. *Control plane* (`bins/server/src/vol_agent.rs`, `crates/workspaces/src/lease.rs`, `model.rs`): a new `POST /vol-agent/jobs/{id}/renew` route, a per-volume in-flight guard in the leasing loop, and `mark_ws_error`/`mark_env_error` moved into the workspaces crate so the requeue sweep can call them too.

The only new cross-crate interfaces are `Job::volume_key()` (model.rs) and the two `lease::mark_*_error` helpers. Nothing here touches the git/registry ownership routing, `BROWSE_TAILS`, or blob deletion.

**Tech Stack:** Rust, tokio, axum 0.8, reqwest, serde_json, chrono, Cosmos DB (`crates/workspaces/src/cosmos.rs`) with `store::MemStore` in tests, btrfs shell-outs.

**Audit findings covered** (`docs/superpowers/audit-2026-08-25.md`): H6a, H6b, H2 (btrfs/engine half only — container-level idempotency and shell-out timeouts are a separate runtime plan), H3, H7, M7. Explicitly out of scope: C1, H1, H4, H5, P1–P7.

## Global Constraints

- **Never lose unpushed data.** Every change here is judged on that single rule. `unpushed` lineage marks and their staged blobs are the ONLY copy of that data — no change may widen a window where a crash, a janitor tick or a replay can drop one. Two of these tasks ship a crash-simulation test for exactly that.
- `cargo clippy --workspace -- -D warnings` green after every task.
- `cargo test` green before every commit (run the narrow `--test`/`-p` command while iterating).
- Comments explain WHY, never what; match the density of the file being edited.
- Preserve every `// ponytail:` marker near an edit; adjust one only where the task removes the ceiling it names, and add one where a task cuts a corner with a known ceiling.
- Commit subjects are imperative sentence case, no tool attribution, no Claude reference.
- btrfs-touching tests stay gated on `kloudlite_git_workspaces::engine::have_btrfs()` and skip cleanly (this Mac and non-root CI are not root-capable btrfs hosts). Any test that must run everywhere must not shell out to `btrfs`.
- Line numbers below were verified against the working tree at plan time — re-read the quoted anchor before editing; if it moved, follow the quote, not the number.

## File Structure

| File | Responsibility in this plan |
|---|---|
| `crates/workspaces/src/engine/pool.rs` | `set_lineage` durable write (tmp+rename, `Result`). Unit tests for the crash seam. |
| `crates/workspaces/src/engine/ops.rs` | 8 `set_lineage` call sites propagate the new `Result`; `create_subvol` and `clone_local_snapshot`/`clone_running_local` tolerate an already-existing `live` subvolume (H2 replay idempotency). |
| `crates/workspaces/src/model.rs` | New `Job::volume_key()` — the `(owner, volume-id)` a job mutates, used by the per-volume leasing guard (M7). |
| `crates/workspaces/src/lease.rs` | Requeue sweep; gains `mark_ws_error`/`mark_env_error` (moved from `bins/server`) and calls them on the retry-exhausted branch (H7). |
| `bins/server/src/vol_agent.rs` | `work` skips a queued job whose volume already has a leased job (M7); new `job_renew` handler + route + `vol_agent_job_shape` entry (H2); `job_failed` now calls the moved `lease::mark_*_error`. |
| `bins/agent/src/lib.rs` | Per-job lease-renewal heartbeat (H2); stage sweep age floor (H6b); `EnvUp` recovery keyed on registry history, dead argument-swapped `pull_env` call deleted (H3). |
| `crates/workspaces/tests/engine_pool.rs` | Crash-simulation tests for the lineage write. |
| `bins/agent/tests/loop.rs` | Existing agent-loop harness; renewal test lands here only if the btrfs gate allows — otherwise the renewal test is a `vol_agent.rs` unit test against `MemStore`. |

---

### Task 1: Durable lineage writes (H6a)

**Files:**
- Modify: `crates/workspaces/src/engine/pool.rs` (`set_lineage`, line 53)
- Modify: `crates/workspaces/src/engine/ops.rs` (call sites at lines 231, 315, 487, 530, 562, 621, 713, 883)
- Modify: `bins/agent/src/lib.rs` (test call site, line 788)
- Modify: `crates/workspaces/tests/engine_pool.rs` (new tests)

**Context:** `set_lineage` is `fs::write(...).unwrap()` — truncate-then-write. A crash between the truncate and the write leaves a zero-length `.lineage`, which `Pool::lineage` happily parses as "no entries at all". The `unpushed` marks are the only record that staged data exists, so the next janitor tick sweeps those staged blobs as unreferenced and the data is gone. The `unwrap` also panics the whole agent process on ENOSPC — on the one box that is mid-`btrfs send`.

**Interfaces:**

```rust
// crates/workspaces/src/engine/pool.rs
pub fn set_lineage(&self, name: &str, l: &[LineageEntry]) -> Result<(), String>
```

**Every caller to update** (all 9, verified by `grep -rn "set_lineage" --include="*.rs" .`):

| Site | Enclosing fn | Fix |
|---|---|---|
| `ops.rs:231` | `commit_core` → `Result<String, EngErr>` | `.map_err(EngErr::other)?` |
| `ops.rs:315` | `upload_core` → `Result<PushOut, EngErr>` | `.map_err(EngErr::other)?` |
| `ops.rs:487` | `pull_core` → `Result<PullOut, EngErr>` | `.map_err(EngErr::other)?` |
| `ops.rs:530` | `inherit` → `Result<Vec<LineageEntry>, EngErr>` | `.map_err(EngErr::other)?` |
| `ops.rs:562` | `restore` → `Result<(), EngErr>` | `.map_err(EngErr::other)?` |
| `ops.rs:621` | `clone_local_snapshot` → `Result<(), EngErr>` | `.map_err(EngErr::other)?` |
| `ops.rs:713` | closure inside `clone_running_local` → `Result<(), EngErr>` | `.map_err(EngErr::other)?` |
| `ops.rs:883` | `squash_inner` → `Result<(), EngErr>` | `.map_err(EngErr::other)?` |
| `bins/agent/src/lib.rs:788` | `janitor_tests::keeps_only_tip_and_unpushed_reclaims_the_rest` | `.unwrap()` (test) |

Also `docs/superpowers/poc/wssnap/main.rs` has its own copy — it is a POC document, not compiled. Leave it.

- [ ] **Step 1: Failing test — a crash mid-write must never truncate the lineage.** Append to `crates/workspaces/tests/engine_pool.rs`:

```rust
#[test]
fn set_lineage_is_atomic_and_leaves_no_partial_file() {
    let tmp = tempfile::tempdir().unwrap();
    let pool = Pool::new(tmp.path());
    std::fs::create_dir_all(pool.root.join("vol")).unwrap();
    let e = |b: &str, unpushed: bool| LineageEntry {
        kind: LayerKind::Stream,
        blob: b.into(),
        snap: None,
        sha256: "sha".into(),
        unpushed,
    };

    pool.set_lineage("v1", &[e("b1", false), e("b2", true)]).unwrap();
    assert_eq!(pool.lineage("v1").len(), 2);

    // Simulate a crash of a PREVIOUS write: a stale tmp file left behind must neither be read
    // back as the lineage nor block the next write from succeeding.
    let stale = pool.root.join("vol").join("v1.lineage.tmp");
    std::fs::write(&stale, b"s:garbage").unwrap();
    pool.set_lineage("v1", &[e("b1", false), e("b2", true), e("b3", true)]).unwrap();

    let back = pool.lineage("v1");
    assert_eq!(back.len(), 3, "a stale tmp file must not corrupt the real lineage");
    assert_eq!(back.iter().filter(|x| x.unpushed).count(), 2, "unpushed marks survive the write");
    assert!(!stale.exists(), "the tmp file is renamed away, never left behind");
}

#[test]
fn set_lineage_returns_err_instead_of_panicking_on_an_unwritable_pool() {
    let tmp = tempfile::tempdir().unwrap();
    let pool = Pool::new(tmp.path());
    // `vol/` deliberately absent: the write fails, and the caller must get an Err — a panic here
    // takes the whole agent down mid-push (the ENOSPC shape of H6a).
    let err = pool.set_lineage("v1", &[]).unwrap_err();
    assert!(!err.is_empty());
}
```

- [ ] **Step 2: Run it, watch it fail.** `cargo test -p kloudlite-git-workspaces --test engine_pool set_lineage` — expect a compile error (`set_lineage` returns `()`, `.unwrap()` invalid), which is the failure.

- [ ] **Step 3: Implement.** Replace `pool.rs:53-56` with:

```rust
    /// tmp+rename, never truncate-in-place: this file's `unpushed` marks are the ONLY record
    /// that staged data exists, so a half-written `.lineage` reads back as "no entries" and the
    /// janitor then sweeps the only copy of that data (audit H6a). Returns `Result` rather than
    /// unwrapping because the caller is usually mid-push on a box that just hit ENOSPC — a panic
    /// there kills every other in-flight job on the agent too.
    pub fn set_lineage(&self, name: &str, l: &[LineageEntry]) -> Result<(), String> {
        let s: Vec<String> = l.iter().map(LineageEntry::encode).collect();
        let final_path = self.root.join("vol").join(format!("{name}.lineage"));
        let tmp = self.root.join("vol").join(format!("{name}.lineage.tmp"));
        std::fs::write(&tmp, s.join("\n")).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &final_path).map_err(|e| format!("{}: {e}", final_path.display()))
    }
```

Then apply the 9 caller edits from the table above.

- [ ] **Step 4: Run it, watch it pass.** `cargo test -p kloudlite-git-workspaces --test engine_pool` then `cargo test` and `cargo clippy --workspace -- -D warnings`.

- [ ] **Step 5: Commit.**
```sh
git add -A && git commit -m "Write the pool lineage file with tmp+rename and propagate its errors"
```

---

### Task 2: The janitor cannot sweep a live stage file (H6b)

**Files:**
- Modify: `bins/agent/src/lib.rs` (`janitor_sweep_stage`, line 294; `spawn_janitor` doc, line 220)

**Context:** `commit_core` writes the staged blob at `ops.rs:206` and only appends the `unpushed` lineage entry at `ops.rs:230` — a window of a whole `btrfs send` + zstd compress, easily minutes on a multi-GB delta. `spawn_janitor` builds its keep-set by reading every volume's lineage with no `ws_lock`, so a tick landing in that window sees the blob referenced by nothing and deletes it. The retried push then fails forever: `upload_core` reads `stage_meta_path` and the file is gone.

**Decision — age floor, not the lock.** Taking `ws_lock` is the wrong tool here for three reasons, and the age floor is strictly smaller:
- The stage dir is **pool-global**, not per-volume. `ws_lock` is per-volume, so to make the sweep safe under locking the janitor would have to hold *every* volume's flock simultaneously for the whole tick — an all-or-nothing lock order that does not exist today and that a pushing agent would deadlock against.
- `ws_lock` is a **blocking** `libc::flock`, and the janitor runs on the shared reactor (`tokio::spawn`, not `spawn_blocking`). Grabbing locks there stalls every other task on the agent; the audit already flags the janitor as reactor-blocking (§3, medium).
- Even a global lock does not close the window: the stage file is written *before* `commit_core` takes anything the sweep could observe, so correctness has to come from "young files are presumed live" either way.

The age floor is one `metadata().modified()` call and is sound because a stage file is only ever swept as *orphan* garbage — a crash leftover. Nothing needs it collected promptly; an hour of latency on reclaiming a crashed push's bytes costs disk, while collecting one second early costs data.

**Interfaces:**

```rust
// bins/agent/src/lib.rs
fn janitor_sweep_stage(engine: &Engine, keep: &std::collections::HashSet<String>) -> usize  // unchanged signature
const STAGE_MIN_AGE: std::time::Duration = std::time::Duration::from_secs(3600);
```

- [ ] **Step 1: Failing test — a freshly staged file with no lineage entry yet must survive.** Add to `mod janitor_tests` in `bins/agent/src/lib.rs`. Note this test must NOT be btrfs-gated: it touches only the stage dir.

```rust
    fn bare_engine(pool_root: std::path::PathBuf) -> Engine {
        Engine::new(
            Pool::new(pool_root),
            std::sync::Arc::new(object_store::memory::InMemory::new()),
            std::sync::Arc::new(MemStore::new()),
            kloudlite_git_workspaces::registry_client::RegistryClient::new("http://127.0.0.1:1", "unused"),
        )
    }

    /// The H6b race, reproduced without btrfs: `commit_core` has written the staged blob but has
    /// not appended its lineage entry yet, so the keep-set legitimately does not name it. A
    /// janitor tick in that window must not delete the only copy of that data.
    #[test]
    fn stage_sweep_spares_a_file_staged_seconds_ago_with_no_lineage_entry_yet() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = bare_engine(tmp.path().to_path_buf());
        std::fs::create_dir_all(engine.pool.stage_dir()).unwrap();
        std::fs::write(engine.pool.stage_path("mid-push"), b"layer bytes").unwrap();
        std::fs::write(engine.pool.stage_meta_path("mid-push"), b"{}").unwrap();

        let keep = std::collections::HashSet::new();
        assert_eq!(janitor_sweep_stage(&engine, &keep), 0, "a young stage file is presumed live");
        assert!(engine.pool.stage_path("mid-push").exists());
        assert!(engine.pool.stage_meta_path("mid-push").exists());
    }

    /// The other half of the contract: a genuinely orphaned file, old enough that no push can
    /// still be mid-flight for it, is still reclaimed.
    #[test]
    fn stage_sweep_still_reclaims_an_old_orphan() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = bare_engine(tmp.path().to_path_buf());
        std::fs::create_dir_all(engine.pool.stage_dir()).unwrap();
        let p = engine.pool.stage_path("crashed-push");
        std::fs::write(&p, b"orphan").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        filetime::set_file_mtime(&p, filetime::FileTime::from_system_time(old)).unwrap();

        assert_eq!(janitor_sweep_stage(&engine, &std::collections::HashSet::new()), 1);
        assert!(!p.exists());
    }

    /// Crash-simulation for H6a+H6b together: an empty `.lineage` (what a truncate-then-write
    /// crash used to leave) makes the keep-set empty, and the sweep must STILL not delete the
    /// staged blobs that lineage was supposed to name.
    #[test]
    fn an_empty_lineage_file_does_not_let_the_sweep_delete_staged_blobs() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = bare_engine(tmp.path().to_path_buf());
        std::fs::create_dir_all(engine.pool.root.join("vol").join("v1")).unwrap();
        std::fs::write(engine.pool.root.join("vol").join("v1.lineage"), b"").unwrap();
        std::fs::create_dir_all(engine.pool.stage_dir()).unwrap();
        std::fs::write(engine.pool.stage_path("b1"), b"only copy").unwrap();

        let keep: std::collections::HashSet<String> =
            engine.pool.lineage("v1").iter().filter(|e| e.unpushed).map(|e| e.blob.clone()).collect();
        assert!(keep.is_empty(), "a truncated lineage really does yield an empty keep-set");
        assert_eq!(janitor_sweep_stage(&engine, &keep), 0);
        assert!(engine.pool.stage_path("b1").exists(), "unpushed data survives a truncated lineage");
    }
```

`filetime` is needed as a dev-dependency of `bins/agent`. Add to `bins/agent/Cargo.toml` under `[dev-dependencies]`: `filetime = "0.2"`. (Check `cargo tree -p kloudlite-git-agent-bin | grep filetime` first — if it is already in the lock graph, pin the same version.)

- [ ] **Step 2: Run it, watch it fail.** `cargo test -p kloudlite-git-agent-bin --lib janitor_tests` — `stage_sweep_spares_a_file_staged_seconds_ago_with_no_lineage_entry_yet` and `an_empty_lineage_file_...` fail (sweep returns 1, files gone).

- [ ] **Step 3: Implement.** Replace `janitor_sweep_stage` (lib.rs:290-305) with:

```rust
/// A stage file is only ever swept as ORPHAN garbage — a crash leftover between staging and
/// push clearing it. Nothing needs that reclaimed promptly, so anything younger than this is
/// presumed to belong to a push still in flight and left alone. `Engine::commit_core` writes the
/// staged blob (`ops.rs`) BEFORE appending its `unpushed` lineage entry, and this sweep computes
/// its keep-set from lineage files alone — without this floor, a tick landing in that window
/// deletes the only copy of freshly staged data and the retried push then fails forever on the
/// missing stage file (audit H6b). An age floor, not `ws_lock`: the stage dir is pool-global
/// while the flock is per-volume, and the janitor runs on the shared reactor where a blocking
/// flock would stall every other job.
const STAGE_MIN_AGE: std::time::Duration = std::time::Duration::from_secs(3600);

/// Removes any staged layer/meta file (`{blob}.zst`/`{blob}.json` under `Pool::stage_dir`) whose
/// blob id isn't in `keep` AND which is older than `STAGE_MIN_AGE` — orphaned by a crash between
/// staging and push clearing it, since a clean push already deletes its own. Global (not
/// per-volume): the stage dir is shared pool state, so `keep` must already be the union across
/// every volume's unpushed entries.
fn janitor_sweep_stage(engine: &Engine, keep: &std::collections::HashSet<String>) -> usize {
    let mut swept = 0;
    let Ok(entries) = std::fs::read_dir(engine.pool.stage_dir()) else { return 0 };
    for entry in entries.flatten() {
        let p = entry.path();
        let Some(stem) = p.file_stem().map(|s| s.to_string_lossy().to_string()) else { continue };
        if keep.contains(&stem) {
            continue;
        }
        // Unreadable mtime => treat as young: keeping a file costs disk, deleting one costs data.
        let young = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|m| m.elapsed().map(|e| e < STAGE_MIN_AGE).unwrap_or(true))
            .unwrap_or(true);
        if young {
            continue;
        }
        if std::fs::remove_file(&p).is_ok() {
            swept += 1;
        }
    }
    swept
}
```

Then extend `spawn_janitor`'s doc comment (lib.rs:220-227) with one sentence naming the floor.

- [ ] **Step 4: Run it, watch it pass.** `cargo test -p kloudlite-git-agent-bin --lib janitor_tests` then `cargo clippy --workspace -- -D warnings`.

- [ ] **Step 5: Commit.**
```sh
git add -A && git commit -m "Give the janitor stage sweep an age floor so it cannot delete a live stage file"
```

---

### Task 3: Idempotent engine replays (H2, btrfs half)

**Files:**
- Modify: `crates/workspaces/src/engine/ops.rs` (`create_subvol` line 166; `clone_local_snapshot` line 610; `clone_running_local` line 692)
- Modify: `crates/workspaces/tests/engine_ops.rs` (new btrfs-gated tests)

**Context:** A lease expiry requeues a still-running job back to the same agent, so `WsCreate`/`EnvUp`/`WsClone` re-run against a pool where their subvolume already exists. `btrfs subvolume create` on an existing path errors; so does `btrfs subvolume snapshot` into an existing destination. Three such "failures" exhaust the retry budget and mark a perfectly healthy workspace `Error`. Task 4 makes the requeue far rarer; this makes the replay harmless when it still happens.

**Interfaces:** unchanged signatures — `create_subvol(&self, id: &str) -> Result<(), EngErr>`, `clone_local_snapshot(&self, src_id: &str, dst_id: &str) -> Result<(), EngErr>`, `clone_running_local(...) -> Result<CloneOut, EngErr>`.

- [ ] **Step 1: Failing tests.** Add to `crates/workspaces/tests/engine_ops.rs` (follow the file's existing `LoopbackPool` + `have_btrfs()` gate idiom):

```rust
#[tokio::test]
async fn create_subvol_is_idempotent_across_a_replayed_job() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let engine = engine_for(&lp);
    engine.create_subvol("vol-replay-1").unwrap();
    std::fs::write(engine.pool.live("vol-replay-1").join("keep-me"), b"user data").unwrap();

    // The lease expired mid-job and the same agent got the job back.
    engine.create_subvol("vol-replay-1").expect("a replayed create must not error");
    assert!(
        engine.pool.live("vol-replay-1").join("keep-me").exists(),
        "a replay must never wipe the subvolume it found"
    );
}

#[tokio::test]
async fn clone_into_an_existing_live_subvolume_is_idempotent() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let engine = engine_for(&lp);
    engine.create_subvol("src-1").unwrap();
    std::fs::write(engine.pool.live("src-1").join("f"), b"x").unwrap();

    engine.clone_local_ids("owner", "src-1", "dst-1").await.unwrap();
    assert!(engine.pool.live("dst-1").join("f").exists());
    engine
        .clone_local_ids("owner", "src-1", "dst-1")
        .await
        .expect("a replayed clone must not error on an existing dst live subvolume");
    assert!(engine.pool.live("dst-1").join("f").exists());
}
```

(`engine_for(&lp)` — reuse the helper already in that file; if it is named differently there, use the existing one rather than adding a second.)

- [ ] **Step 2: Run it, watch it fail.** On a btrfs-capable Linux box, as root: `cargo test -p kloudlite-git-workspaces --test engine_ops idempotent`. On this Mac the gate skips — implement against the code review, and note in the commit that the assertion ran in the VM. If no VM is available, the fallback verification is the shell one-liner in Step 4.

- [ ] **Step 3: Implement.** In `create_subvol` (ops.rs:166):

```rust
    pub fn create_subvol(&self, id: &str) -> Result<(), EngErr> {
        std::fs::create_dir_all(self.pool.voldir(id)).map_err(EngErr::io)?;
        // A lease that expired mid-job sends the SAME job back to the SAME agent, so this runs a
        // second time against a subvolume it already made (audit H2). Existing == done: never
        // re-create (which errors) and never delete-and-recreate (which would throw away whatever
        // the first attempt already wrote into it).
        if !self.pool.live(id).exists() {
            run(&["btrfs", "subvolume", "create", self.pool.live(id).to_str().unwrap()])?;
        }
        std::fs::create_dir_all(self.pool.recv()).map_err(EngErr::io)?;
        Ok(())
    }
```

In `clone_local_snapshot`, guard the `run(&["btrfs", "subvolume", "snapshot", ...])` at ops.rs:624-630:

```rust
        // Same replayed-job tolerance as `create_subvol`: `dst`'s live already existing means a
        // prior attempt of THIS job got here. The lineage file is rewritten above either way, so
        // re-pointing it is free; re-snapshotting is not (it errors, and would clobber).
        if !self.pool.live(dst_id).exists() {
            run(&[
                "btrfs",
                "subvolume",
                "snapshot",
                tip_snap.to_str().unwrap(),
                self.pool.live(dst_id).to_str().unwrap(),
            ])?;
        }
```

In `clone_running_local`'s closure, guard the snapshot at ops.rs:706-712 identically.

`pull_core` (ops.rs:476) already has this shape (`if !self.pool.live(name).exists()`) — leave it, and match its wording.

- [ ] **Step 4: Run it, watch it pass.** `cargo test -p kloudlite-git-workspaces` plus, on a btrfs host, `cargo test -p kloudlite-git-workspaces --test engine_ops idempotent`. Cross-check the guard by hand where btrfs is unavailable:
```sh
grep -n "subvolume\", \"create\"\|subvolume\", \"snapshot\"" crates/workspaces/src/engine/ops.rs
```
every `create`/`snapshot` into a `live(...)` path must sit under an `.exists()` guard.

- [ ] **Step 5: Commit.**
```sh
git add -A && git commit -m "Tolerate an existing live subvolume on a replayed create or clone"
```

---

### Task 4: Per-job lease renewal (H2)

**Files:**
- Modify: `bins/server/src/vol_agent.rs` (`vol_agent_job_shape` line 61; new `job_renew`; `vol_agent_job_routes` line 624; `LEASE_SECS` const)
- Modify: `bins/agent/src/lib.rs` (`run_with_engine`'s spawn block, line 208)

**Context:** The lease is a flat 120s stamped at lease time (`vol_agent.rs:400`) and nothing ever extends it. A `btrfs send` of a multi-GB delta, an Azure upload, or a `docker` image pull each blow through 120s routinely. The sweep then requeues a job that is still running, the owner binding sends it straight back to the same agent, and the job runs concurrently with itself. Task 3 makes that survivable; this makes it not happen.

**Interfaces:**

```rust
// bins/server/src/vol_agent.rs
/// How long a lease is stamped for. The agent renews at a third of this while the job runs.
const LEASE_SECS: i64 = 120;
async fn job_renew(
    Extension(s): Extension<Arc<JobsState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(hint): Query<RegionHint>,
) -> Result<Response, Response>;
// route: POST /vol-agent/jobs/{id}/renew

// bins/agent/src/lib.rs
/// Renews `job_id`'s lease every `RENEW_EVERY` until dropped. Returns the guard task's handle.
fn spawn_lease_renewal(
    client: reqwest::Client,
    api: String,
    token: String,
    job_id: String,
) -> tokio::task::JoinHandle<()>;
const RENEW_EVERY: std::time::Duration = std::time::Duration::from_secs(40);
```

- [ ] **Step 1: Failing test — renew extends the lease, and only for the leaseholder.** Add to `mod tests` in `bins/server/src/vol_agent.rs`:

```rust
    use kloudlite_git_workspaces::store::MemStore;

    fn leased_job(region: &str, agent: &str, until: chrono::DateTime<chrono::Utc>) -> Job {
        Job {
            id: "job-renew-1".into(),
            region: region.into(),
            agent: Some(agent.into()),
            kind: JobKind::WsPush,
            payload: serde_json::json!({"owner": "A", "workspace": "ws-1"}),
            state: JobState::Leased,
            lease_until: Some(until),
            attempts: 0,
            error: None,
        }
    }

    #[tokio::test]
    async fn renew_extends_only_the_current_holders_lease() {
        let store = MemStore::new();
        let soon = chrono::Utc::now() + chrono::Duration::seconds(5);
        store.create_job(&leased_job("r1", "a1", soon)).await.unwrap();

        assert!(renew_lease(&store, "r1", "job-renew-1", "a1").await, "the holder renews");
        let (j, _) = store.get_job("r1", "job-renew-1").await.unwrap().unwrap();
        assert!(j.lease_until.unwrap() > soon + chrono::Duration::seconds(60));
        assert_eq!(j.state, JobState::Leased);

        // A stale attempt-1 agent must not be able to hold a lease attempt 2 now owns.
        assert!(!renew_lease(&store, "r1", "job-renew-1", "someone-else").await);
    }

    #[tokio::test]
    async fn renew_refuses_a_job_that_is_no_longer_leased() {
        let store = MemStore::new();
        let mut j = leased_job("r1", "a1", chrono::Utc::now());
        j.state = JobState::Done;
        store.create_job(&j).await.unwrap();
        assert!(!renew_lease(&store, "r1", "job-renew-1", "a1").await);
    }
```

- [ ] **Step 2: Run it, watch it fail.** `cargo test -p kloudlite-git-server --lib vol_agent::tests::renew` — fails to compile (`renew_lease` does not exist).

- [ ] **Step 3: Implement — server side.** In `vol_agent.rs`, add near `JobsState`:

```rust
/// How long a lease is stamped for. Unchanged from the pre-renewal 120s: the fix is that the
/// agent now RENEWS it (`spawn_lease_renewal` in `bins/agent`) rather than letting a multi-minute
/// `btrfs send` or image pull run past it, which requeued a still-running job and ran it
/// concurrently with itself (audit H2).
const LEASE_SECS: i64 = 120;
```

Replace the literal at `vol_agent.rs:400` with `chrono::Duration::seconds(LEASE_SECS)`. Add the core, kept separate from the handler so it is unit-testable against `MemStore`:

```rust
/// Extends a leased job's `lease_until`, but ONLY for the agent that currently holds it and only
/// while it is still `Leased` — a late renewal from a superseded attempt must never re-take a job
/// that was requeued and re-leased elsewhere. `false` (a 409 to the caller) on any of those.
async fn renew_lease(store: &dyn MetaStore, region: &str, id: &str, agent: &str) -> bool {
    let Ok(Some((mut job, etag))) = store.get_job(region, id).await else { return false };
    if job.state != JobState::Leased || job.agent.as_deref() != Some(agent) {
        return false;
    }
    job.lease_until = Some(chrono::Utc::now() + chrono::Duration::seconds(LEASE_SECS));
    store.replace_job(&job, &etag).await.is_ok()
}

#[derive(serde::Deserialize)]
struct RenewQuery {
    agent: String,
    #[serde(default)]
    region: Option<String>,
}

async fn job_renew(
    Extension(s): Extension<Arc<JobsState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<RenewQuery>,
) -> Result<Response, Response> {
    let store = require_store(&s)?;
    let region = region_by_token(&**store, &headers, q.region.as_deref()).await?;
    if renew_lease(&**store, &region.id, &id, &q.agent).await {
        Ok(StatusCode::OK.into_response())
    } else {
        // Not an error the agent can fix by retrying — it no longer holds this job.
        Ok((StatusCode::CONFLICT, "not the current leaseholder").into_response())
    }
}
```

Mount it in `vol_agent_job_routes()`:
```rust
        .route("/vol-agent/jobs/{id}/renew", post(job_renew))
```
and widen `vol_agent_job_shape` (line 72) so the pre-auth router lets it through:
```rust
    matches!(
        (it.next(), it.next(), it.next()),
        (Some(_id), Some("done" | "failed" | "renew"), None)
    )
```

- [ ] **Step 4: Run the server tests, watch them pass.** `cargo test -p kloudlite-git-server --lib vol_agent`.

- [ ] **Step 5: Implement — agent side.** In `bins/agent/src/lib.rs`, add:

```rust
/// Renew at a third of the server's `LEASE_SECS` (120s): two renewals may be lost to a blip
/// before the lease actually lapses.
const RENEW_EVERY: std::time::Duration = std::time::Duration::from_secs(40);

/// Holds `job_id`'s lease open for as long as the job runs. Dropped (aborted) the moment the job
/// finishes, so a completed job's lease is never extended past its report. Best-effort: a failed
/// renewal just means the next tick tries again, and a 409 means we are no longer the holder —
/// nothing to do about it here, the report will be refused too.
fn spawn_lease_renewal(
    client: reqwest::Client,
    api: String,
    token: String,
    agent_id: String,
    job_id: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let url = format!("{api}/vol-agent/jobs/{job_id}/renew?agent={agent_id}");
        loop {
            tokio::time::sleep(RENEW_EVERY).await;
            match client.post(&url).header(kloudlite_git_workspaces::api::WS_AGENT_HEADER, &token).send().await {
                Ok(r) if r.status() == reqwest::StatusCode::CONFLICT => {
                    eprintln!("agent: job {job_id} lease lost (no longer the holder)"); // ponytail: eprintln
                    return;
                }
                Ok(_) => {}
                Err(e) => eprintln!("agent: renewing lease for job {job_id}: {e}"), // ponytail: eprintln
            }
        }
    })
}
```

Wire it into the spawn block at lib.rs:208-216, so it starts before the blocking job and is aborted as soon as the job returns (before `report`):

```rust
        let agent_id_for_job = agent_id.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let job_id = job.id.clone();
            // The lease must outlive the job, not the other way round: a multi-GB `btrfs send`
            // takes minutes and the flat 120s lease used to lapse under it (audit H2).
            let renew = spawn_lease_renewal(
                client.clone(),
                cfg_api.clone(),
                cfg_tok.clone(),
                agent_id_for_job,
                job_id.clone(),
            );
            let outcome = tokio::task::spawn_blocking(move || run_job_blocking(&engine, &job))
                .await
                .unwrap_or_else(|e| Err(format!("job panicked: {e}")));
            renew.abort();
            inflight.fetch_sub(1, Ordering::Relaxed);
            report(&client, &cfg_api, &cfg_tok, &job_id, outcome).await;
        });
```

- [ ] **Step 6: Run everything.** `cargo test` and `cargo clippy --workspace -- -D warnings`. On a btrfs host also `cargo test -p kloudlite-git-agent-bin --test loop`.

- [ ] **Step 7: Commit.**
```sh
git add -A && git commit -m "Renew a job's lease from the agent while the job is still running"
```

---

### Task 5: Environment re-materialization keyed on registry history (H3)

**Files:**
- Modify: `bins/agent/src/lib.rs` (`JobKind::EnvUp` arm, lines 509-525)
- Modify: `crates/workspaces/src/model.rs` (`Workspace.volume` / `Environment.volume` doc comments, lines 86 and 232)

**Context:** `EnvUp`'s recovery branch reads `env.volume` — a field nothing in production ever writes, despite `model.rs:86` claiming the job-done handler does. So an environment whose local subvolume is gone (pool wipe, node replacement) always falls into the `None` arm and is rebuilt **empty**, even though its whole pushed history is sitting in the registry. To the user the data is simply gone. And the branch that was supposed to save them is itself broken: `engine.pull_env(&env.id, r)` passes `(id, volume-pointer)` to a function whose signature is `pull_env(owner, id)`.

**Decision — key recovery off "registry history non-empty", drop the pointer read.** Writing the pointer in `job_done` would mean:
- adding a fourth `mark_*` helper and a new `volume`-writing CAS path to a handler already doing three,
- keeping a Cosmos field in sync with registry state that is *already* the truth — a second source of truth for "has this env ever pushed", which can go stale exactly when it matters (a `job_done` that never lands after a successful push),
- and it still would not help an env whose Cosmos doc predates the write.

`pull_env` already asks the registry and already fails clean with `"environment has no history; push first"` on a never-pushed volume. That error IS the signal. Recovery becomes: try `pull_env(owner, id)`; if the registry has no history, create a fresh subvolume. One branch, no new state, correct for old docs. The `volume` field stays on the model (docs in the wild have it, `api.rs` surfaces it in `VolumeSummary`) but its doc comment stops claiming something is writing it.

**Interfaces:** none changed. `Engine::pull_env(&self, owner: &str, id: &str) -> Result<PullOut, EngErr>` is called correctly for the first time.

- [ ] **Step 1: Failing test — an env with pushed history re-materializes instead of coming up empty.** Add to `bins/agent/tests/loop.rs` (btrfs-gated, alongside the existing WsCreate/WsPush drive):

```rust
/// H3: an environment whose local subvolume is gone (pool wipe / node replacement) but whose
/// history is in the registry must come back with its data, not as a fresh empty subvolume.
#[tokio::test]
async fn env_up_rematerializes_from_registry_history_after_the_subvolume_is_gone() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let (lp, engine, _base, store) = env_harness().await;

    // An environment that has pushed once.
    engine.create_subvol("env-1").unwrap();
    std::fs::write(engine.pool.live("env-1").join("data.txt"), b"user data").unwrap();
    engine.push_env("A", "env-1", &serde_json::Value::Null, Some("initial")).await.unwrap();
    assert!(!store.history("A", "env-1").await.unwrap().is_empty());

    // The pool loses the subvolume. `Environment.volume` is None — as it is for EVERY env in
    // production, which is exactly why the old pointer-keyed branch was dead code.
    run(&["btrfs", "subvolume", "delete", engine.pool.live("env-1").to_str().unwrap()]);
    assert!(!engine.pool.live("env-1").exists());

    recover_env_volume(&engine, "A", "env-1").await.unwrap();
    assert_eq!(
        std::fs::read(engine.pool.live("env-1").join("data.txt")).unwrap(),
        b"user data",
        "a pushed environment must never be silently rebuilt empty"
    );
}

/// The other arm: a never-pushed environment still gets a fresh subvolume, not an error.
#[tokio::test]
async fn env_up_creates_a_fresh_subvolume_when_there_is_no_history() {
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let (_lp, engine, _base, _store) = env_harness().await;
    recover_env_volume(&engine, "A", "env-never-pushed").await.unwrap();
    assert!(engine.pool.live("env-never-pushed").exists());
}
```

(`env_harness()` builds the same in-process vol-agent server + `LoopbackPool` + `Engine` the file's existing test already assembles — extract it from that test rather than duplicating the setup.)

- [ ] **Step 2: Run it, watch it fail.** `cargo test -p kloudlite-git-agent-bin --test loop env_up_` — fails to compile (`recover_env_volume` does not exist). On a non-btrfs machine the gate skips; run this one on the Linux VM.

- [ ] **Step 3: Implement.** Add to `bins/agent/src/lib.rs`, next to the other job helpers:

```rust
/// Materialize an environment's live subvolume when it isn't on this pool: from the registry when
/// the env has pushed history (a pool wipe or node replacement must not lose pushed data), else a
/// fresh empty subvolume (a genuinely new env).
///
/// Keyed on the REGISTRY, not on `Environment.volume`: nothing has ever written that pointer, so
/// the branch that read it was dead code and silently rebuilt pushed environments empty (audit
/// H3). The registry is already the truth for "has this volume ever pushed" — a Cosmos pointer
/// would be a second copy of that fact, stale in exactly the case that matters.
pub async fn recover_env_volume(engine: &Engine, owner: &str, id: &str) -> Result<(), String> {
    match engine.pull_env(owner, id).await {
        Ok(_) => Ok(()),
        // `pull_env`'s own "never pushed" error is the signal that there is nothing to recover.
        Err(e) if e.to_string().contains("no history") => engine.create_subvol(id).map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}
```

Replace the `EnvUp` arm's recovery block (lib.rs:513-520) with:

```rust
            if !live.exists() {
                recover_env_volume(engine, &owner, &env.id).await?;
            }
```

Then fix `model.rs:86` and `model.rs:232` to stop asserting a write that does not happen:

```rust
    /// Pointer to the storage registry volume (`vol/{owner}/{id}`). Informational only and
    /// `None` in practice: nothing writes it (the registry's own history is what
    /// `recover_env_volume` in `bins/agent` keys re-materialization off). Kept because docs in
    /// the wild carry it and `/v1/volumes` surfaces it; `ref` stays as an alias so docs written
    /// before the commit/push split still deserialize.
    /// ponytail: a vestigial field, not a source of truth. Upgrade path: either delete it from
    /// both models and `VolumeSummary`, or make `job_done` write it and make it authoritative —
    /// not the current half-state.
```

- [ ] **Step 4: Run it, watch it pass.** On the btrfs VM: `cargo test -p kloudlite-git-agent-bin --test loop`. Everywhere: `cargo test && cargo clippy --workspace -- -D warnings`.

- [ ] **Step 5: Commit.**
```sh
git add -A && git commit -m "Recover an environment's volume from registry history instead of a dead pointer"
```

---

### Task 6: Sweep-exhausted jobs mark their doc Error (H7)

**Files:**
- Modify: `bins/server/src/vol_agent.rs` (move `mark_ws_error` line 476 and `mark_env_error` line 526 out; update the two `job_failed` call sites at lines 594-595)
- Modify: `crates/workspaces/src/lease.rs` (host the two helpers; call them on the exhausted branch, line 33)

**Context:** `lease.rs:33` flips a job to `JobState::Failed` when its attempts exceed the budget, but only the HTTP `job_failed` path ever marks the workspace/environment doc. A dead agent's `WsCreate` therefore leaves the workspace in `Creating` forever — a permanent UI spinner with no error anywhere. The helpers already exist and already have the right no-op and no-resurrect-a-delete rules; they just live in the wrong crate. Moving them (rather than copying) keeps the two paths from drifting — the root-cause fix, one shared function both callers route through.

**Interfaces:**

```rust
// crates/workspaces/src/lease.rs  (moved verbatim from bins/server/src/vol_agent.rs, now pub)
pub async fn mark_ws_error(store: &dyn MetaStore, kind: JobKind, payload: &serde_json::Value);
pub async fn mark_env_error(store: &dyn MetaStore, kind: JobKind, payload: &serde_json::Value);
```

`bins/server/src/vol_agent.rs`'s `job_failed` then calls `kloudlite_git_workspaces::lease::mark_ws_error(&**store, job.kind, &job.payload).await` and the `mark_env_error` twin. `mark_ws_ready`, `mark_ws_stopped` and `mark_env_state` stay in `vol_agent.rs` — only the sweep needs the error pair.

- [ ] **Step 1: Failing test.** Add to `crates/workspaces/src/lease.rs` a `#[cfg(test)] mod tests`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Capacity, Job, JobKind, Workspace, WsState};
    use crate::store::MemStore;

    fn ws(owner: &str, id: &str, region: &str) -> Workspace {
        Workspace {
            id: id.into(),
            owner: owner.into(),
            name: "web".into(),
            region: region.into(),
            state: WsState::Creating,
            image: "nginx:alpine".into(),
            placement: None,
            volume: None,
            quota_gb: 20,
            live_state: serde_json::Value::Null,
        }
    }

    fn agent(id: &str, region: &str, age_secs: i64) -> AgentDoc {
        AgentDoc {
            id: id.into(),
            region: region.into(),
            hostname: id.into(),
            pool: "/mnt/wspool".into(),
            capacity: Capacity { cpu: 4, mem_mb: 8192, disk_gb: 500 },
            used: Capacity { cpu: 0, mem_mb: 0, disk_gb: 0 },
            heartbeat_at: chrono::Utc::now() - chrono::Duration::seconds(age_secs),
            status: "alive".into(),
        }
    }

    /// H7: a dead agent's WsCreate must not leave the workspace spinning in `Creating` forever.
    #[tokio::test]
    async fn an_exhausted_job_marks_its_workspace_error() {
        let store = MemStore::new();
        store.upsert_agent(&agent("a1", "r1", 300)).await.unwrap(); // long dead
        store.create_ws(&ws("A", "ws-1", "r1")).await.unwrap();
        store
            .create_job(&Job {
                id: "job-1".into(),
                region: "r1".into(),
                agent: Some("a1".into()),
                kind: JobKind::WsCreate,
                payload: serde_json::json!({"owner": "A", "workspace": "ws-1"}),
                state: JobState::Leased,
                lease_until: Some(chrono::Utc::now() - chrono::Duration::seconds(10)),
                attempts: MAX_ATTEMPTS, // one more attempt exhausts it
                error: None,
            })
            .await
            .unwrap();

        sweep(&store, "r1").await.unwrap();

        let (j, _) = store.get_job("r1", "job-1").await.unwrap().unwrap();
        assert_eq!(j.state, JobState::Failed);
        let (w, _) = store.get_ws("A", "ws-1").await.unwrap().unwrap();
        assert_eq!(w.state, WsState::Error, "the doc must not stay in Creating forever");
    }

    /// A requeue that still has budget left must NOT touch the doc — the job is going to run again.
    #[tokio::test]
    async fn a_requeued_job_leaves_its_workspace_alone() {
        let store = MemStore::new();
        store.upsert_agent(&agent("a1", "r1", 300)).await.unwrap();
        store.create_ws(&ws("A", "ws-1", "r1")).await.unwrap();
        store
            .create_job(&Job {
                id: "job-1".into(),
                region: "r1".into(),
                agent: Some("a1".into()),
                kind: JobKind::WsCreate,
                payload: serde_json::json!({"owner": "A", "workspace": "ws-1"}),
                state: JobState::Leased,
                lease_until: Some(chrono::Utc::now() - chrono::Duration::seconds(10)),
                attempts: 0,
                error: None,
            })
            .await
            .unwrap();

        sweep(&store, "r1").await.unwrap();

        let (j, _) = store.get_job("r1", "job-1").await.unwrap().unwrap();
        assert_eq!(j.state, JobState::Queued);
        let (w, _) = store.get_ws("A", "ws-1").await.unwrap().unwrap();
        assert_eq!(w.state, WsState::Creating);
    }
}
```

- [ ] **Step 2: Run it, watch it fail.** `cargo test -p kloudlite-git-workspaces --lib lease::` — `an_exhausted_job_marks_its_workspace_error` fails on the last assert (`Creating` != `Error`).

- [ ] **Step 3: Implement.** Cut `mark_ws_error` (vol_agent.rs:474-493) and `mark_env_error` (vol_agent.rs:525-546) into `crates/workspaces/src/lease.rs` verbatim, made `pub`, with their doc comments carried over and their `WsState`/`EnvState`/`StoreErr`/`JobKind` imports adjusted to the crate-local paths (`use crate::model::{EnvState, JobKind, WsState};`, `use crate::store::StoreErr;`). Then in the sweep loop:

```rust
        let exhausted = j.attempts > MAX_ATTEMPTS;
        j.state = if exhausted { JobState::Failed } else { JobState::Queued };
        // Lost the CAS race to a poller finishing/leasing it in the meantime: fine, leave it.
        if meta.replace_job(&j, &etag).await.is_err() {
            continue;
        }
        // A job that ran out of retries here (dead agent, lapsed lease) never reaches the HTTP
        // `job_failed` path, so the workspace/environment doc would otherwise sit in `Creating`
        // forever — a permanent UI spinner with no error (audit H7). Only on the terminal branch:
        // a requeue still has an attempt coming.
        if exhausted {
            mark_ws_error(meta, j.kind, &j.payload).await;
            mark_env_error(meta, j.kind, &j.payload).await;
        }
```

In `bins/server/src/vol_agent.rs`, point `job_failed`'s two calls at the moved helpers:

```rust
    if exhausted {
        kloudlite_git_workspaces::lease::mark_ws_error(&**store, job.kind, &job.payload).await;
        kloudlite_git_workspaces::lease::mark_env_error(&**store, job.kind, &job.payload).await;
    }
```

- [ ] **Step 4: Run it, watch it pass.** `cargo test -p kloudlite-git-workspaces --lib lease::` then `cargo test && cargo clippy --workspace -- -D warnings`.

- [ ] **Step 5: Commit.**
```sh
git add -A && git commit -m "Mark the workspace or environment Error when the requeue sweep exhausts its job"
```

---

### Task 7: One job at a time per volume (M7)

**Files:**
- Modify: `crates/workspaces/src/model.rs` (new `impl Job { pub fn volume_key(&self) }`)
- Modify: `bins/server/src/vol_agent.rs` (`work`'s leasing loop, lines 395-397)

**Context:** The agent's semaphore admits 4 jobs at once and nothing stops two of them being for the SAME volume. `WsPush` and `EnvDown` both call `push_env` on one subvolume; `WsDelete` racing a `Push` is `cleanup_local` deleting a stage file mid-upload. `ws_lock` serializes the lineage read-modify-write, but not the surrounding btrfs work, and it does not exist at all between two *nodes*. Refusing to lease is cheaper than any lock: a job left `Queued` is picked up on the next poll iteration a second later.

**Interfaces:**

```rust
// crates/workspaces/src/model.rs
impl Job {
    /// `owner/volume-id` this job mutates, or `None` when the payload names no volume.
    pub fn volume_key(&self) -> Option<String>;
}
```

- [ ] **Step 1: Failing tests.** In `crates/workspaces/src/model.rs`, add a `#[cfg(test)] mod tests` (or extend the existing one):

```rust
#[cfg(test)]
mod job_key_tests {
    use super::*;

    fn job(kind: JobKind, payload: serde_json::Value) -> Job {
        Job {
            id: "j".into(),
            region: "r1".into(),
            agent: None,
            kind,
            payload,
            state: JobState::Queued,
            lease_until: None,
            attempts: 0,
            error: None,
        }
    }

    #[test]
    fn volume_key_names_the_volume_a_job_mutates() {
        assert_eq!(
            job(JobKind::WsPush, serde_json::json!({"owner": "A", "workspace": "ws-1"})).volume_key(),
            Some("A/ws-1".into())
        );
        assert_eq!(
            job(JobKind::EnvDown, serde_json::json!({"owner": "A", "environment": "env-1"})).volume_key(),
            Some("A/env-1".into())
        );
        // Two jobs on different volumes of the same owner are not the same key.
        assert_ne!(
            job(JobKind::WsPush, serde_json::json!({"owner": "A", "workspace": "ws-1"})).volume_key(),
            job(JobKind::WsPush, serde_json::json!({"owner": "A", "workspace": "ws-2"})).volume_key()
        );
        assert_eq!(job(JobKind::WsPush, serde_json::json!({})).volume_key(), None);
    }
}
```

And in `bins/server/src/vol_agent.rs`'s `mod tests`:

```rust
    #[tokio::test]
    async fn a_volume_with_a_leased_job_yields_no_second_lease() {
        let store = MemStore::new();
        let mut leased = leased_job("r1", "a1", chrono::Utc::now() + chrono::Duration::seconds(60));
        leased.id = "job-1".into();
        leased.payload = serde_json::json!({"owner": "A", "workspace": "ws-1"});
        store.create_job(&leased).await.unwrap();

        // Same volume, queued behind it.
        let mut queued = leased.clone();
        queued.id = "job-2".into();
        queued.state = JobState::Queued;
        queued.agent = Some("a1".into());
        queued.lease_until = None;
        store.create_job(&queued).await.unwrap();

        // A different volume, also queued: this one IS leasable.
        let mut other = queued.clone();
        other.id = "job-3".into();
        other.payload = serde_json::json!({"owner": "A", "workspace": "ws-2"});
        store.create_job(&other).await.unwrap();

        let busy = busy_volume_keys(&store, "r1").await;
        let queued_jobs = store.queued_jobs("r1").await.unwrap();
        let pick = pick_leasable(queued_jobs, "a1", &busy).map(|(j, _)| j.id);
        assert_eq!(pick, Some("job-3".into()), "the busy volume's job must be skipped, not the other one");
    }
```

- [ ] **Step 2: Run them, watch them fail.** `cargo test -p kloudlite-git-workspaces --lib job_key_tests` and `cargo test -p kloudlite-git-server --lib vol_agent::tests::a_volume` — both fail to compile.

- [ ] **Step 3: Implement.** In `model.rs`, after the `Job` struct:

```rust
impl Job {
    /// The `owner/volume-id` this job mutates — a workspace id or an environment id, which share
    /// one namespace on the pool (`{pool}/vol/{id}`) and one registry keyspace (`vol/{owner}/{id}`),
    /// so one key covers both. `None` when the payload names no volume (nothing to serialize on).
    /// Used by the leasing loop to keep two jobs for one volume from running concurrently.
    pub fn volume_key(&self) -> Option<String> {
        let owner = self.payload.get("owner")?.as_str()?;
        let id = self
            .payload
            .get("workspace")
            .or_else(|| self.payload.get("environment"))?
            .as_str()?;
        Some(format!("{owner}/{id}"))
    }
}
```

In `vol_agent.rs`, add the two helpers (split out of the handler so the test above can drive them):

```rust
/// Volume keys with a job currently leased in `region` — the set `work` refuses to hand a second
/// job out for. A store error yields an EMPTY set deliberately: failing open keeps work flowing,
/// and the per-volume `ws_lock` on the agent still serializes the lineage writes underneath.
async fn busy_volume_keys(store: &dyn MetaStore, region: &str) -> std::collections::HashSet<String> {
    store
        .leased_jobs(region)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(j, _)| j.volume_key())
        .collect()
}

/// First queued job this agent may take: addressed to it (or unclaimed), and for a volume that
/// has no job in flight already. Two jobs for one volume in the agent's 4-way semaphore would
/// otherwise run concurrently — a `WsDelete` stripping a stage file out from under a `Push`, two
/// `push_env`s snapshotting the same subvolume (audit M7). Skipping just leaves the job Queued;
/// the next poll iteration a second later picks it up.
fn pick_leasable(
    queued: Vec<(Job, kloudlite_git_workspaces::store::Etag)>,
    agent: &str,
    busy: &std::collections::HashSet<String>,
) -> Option<(Job, kloudlite_git_workspaces::store::Etag)> {
    queued.into_iter().find(|(j, _)| {
        j.agent.as_deref().is_none_or(|a| a == agent)
            && j.volume_key().is_none_or(|k| !busy.contains(&k))
    })
}
```

Replace `work`'s lines 395-397 with:

```rust
        let queued = store.queued_jobs(&region.id).await.map_err(job_store_err)?;
        let busy = busy_volume_keys(&**store, &region.id).await;
        let mine = pick_leasable(queued, &q.agent, &busy);
```

- [ ] **Step 4: Run them, watch them pass.** `cargo test -p kloudlite-git-workspaces --lib job_key_tests && cargo test -p kloudlite-git-server --lib vol_agent` then `cargo test && cargo clippy --workspace -- -D warnings`.

- [ ] **Step 5: Commit.**
```sh
git add -A && git commit -m "Skip leasing a job whose volume already has one in flight"
```

---

## Final verification

- [ ] `cargo clippy --workspace -- -D warnings`
- [ ] `cargo test`
- [ ] On a root-capable btrfs Linux VM (not this Mac): `cargo test -p kloudlite-git-workspaces --test engine_ops`, `cargo test -p kloudlite-git-agent-bin --test loop`, and `./tests/ws_e2e.sh` (exit 77 means a prerequisite was missing — that is not a pass).
- [ ] `grep -rn "set_lineage" --include="*.rs" crates bins` — every call site handles the `Result`.
- [ ] Confirm no task widened a window on unpushed data: the janitor's keep-set may only grow, `cleanup_local`'s cross-volume guards are untouched, and no new path deletes a stage file.
