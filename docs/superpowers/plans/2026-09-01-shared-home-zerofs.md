# Shared Home on ZeroFS Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the per-node btrfs home volume with a region-shared ZeroFS (S3-backed NFS) home plus node-local caches, deleting the owner→node pin so workspaces schedule on any Synced node.

**Architecture:** One ZeroFS Deployment per region serves an NFS export; every agent mounts it once at `{pool}/homes` and pods hostPath `{pool}/homes/{owner}` at `/home/kl`. Caches, editor servers, shell history and `~/.local/state` stay on a node-local btrfs subvolume, redirected by env vars. The home `Volume` CR, its push/materialize machinery, and `bound_elsewhere` are deleted; the `OwnerBinding` survives de-pinned as the owner's namespace ensurer, reconciled convergently on every node.

**Tech Stack:** Rust (kube-rs controllers, axum), btrfs, ZeroFS over NFSv3, k3s, Azure Blob.

**Spec:** `docs/superpowers/specs/2026-09-01-shared-home-zerofs-design.md`

## Global Constraints

- Deletion order is load-bearing (spec "Deletions, in dependency order"): stop-path gate first, push beat second, materialize third, CRD surfaces fourth, data last. Tasks are numbered in that order — do not reorder.
- Home history and per-home quota are ACCEPTED LOSSES (spec rulings). Do not rebuild either.
- Shell history and `~/.local/state` MUST be node-local; `HISTFILE` must be set explicitly (spec: `ZDOTDIR` points zsh at the shared side).
- Cache redirection is by env var; path mounts only for `.vscode-server` / `.cursor-server`.
- The binding reconciler must be convergent under concurrent execution on two nodes (its objects are cluster-global).
- `OwnerBindingSpec.node_name` and `home_quota_gb` stay in the schema (existing objects must parse) but nothing may read them after Task 7.
- Every task: `cargo test --workspace --locked` green and `cargo clippy --workspace -- -D warnings` clean before its commit. btrfs-gated engine tests only count from a build-0 run (`rsync` → `cargo test --no-run` → `sudo <binary>`).
- Commit subjects: imperative sentence case, no tool attribution.
- CRD schema changes regenerate `deploy/k3s/crds.yaml` via `CRD_REGEN=1 cargo test -p rustic-git-workspaces --test crd_yaml`.

---

### Task 1: ZeroFS deployment and the agent's NFS mount

**Files:**
- Create: `deploy/k3s/zerofs.yaml`
- Modify: `bins/agent/src/lib.rs` (Cfg + startup mount)
- Modify: `deploy/k3s/agent-daemonset.yaml` (env `WS_HOMES_EXPORT`)
- Test: `bins/agent/src/lib.rs` (unit test in-file)

**Interfaces:**
- Produces: `Cfg.homes_export: Option<String>` (NFS `host:/path`), mount at `{pool}/homes`; `pub fn homes_root(pool: &str) -> PathBuf` returning `{pool}/homes`.
- Later tasks rely on: `{pool}/homes/{owner}` existing as a path convention (Task 3 creates per-owner dirs).

- [ ] **Step 1: Write `deploy/k3s/zerofs.yaml`** — Deployment (1 replica, `rustic-git-system` namespace, image pinned to an exact ZeroFS release tag — resolve the current release at implementation time from https://github.com/Barre/ZeroFS and pin it; never `latest`), a ClusterIP Service `zerofs` exposing NFS port 2049, env from a new Secret `zerofs-store` (`AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`/`AWS_ENDPOINT_URL` for the region's Azure blob via its S3-compatible gateway, or Azurite-style credentials — match what the region's blob account offers), `SLATEDB_PREFIX=homes`, resource requests `cpu: 500m, memory: 1Gi`. Header comment: single replica ON PURPOSE — SlateDB is single-writer (writer-epoch fencing, same invariant as the repo databases); a second replica fences the first. Availability ruling from the spec: a down ZeroFS hangs `/home/kl` region-wide until reschedule; accepted.
- [ ] **Step 2: Add `homes_export` to `Cfg`** in `bins/agent/src/lib.rs`:

```rust
// In Cfg:
/// `WS_HOMES_EXPORT`, e.g. `zerofs.rustic-git-system.svc:/` — the region's shared-home NFS
/// export. Unset means no shared home on this node: workspace reconciles that need it park on
/// HomeNotReady (fail closed, same shape as WS_PEER_SECRET gating the peer listener).
pub homes_export: Option<String>,
// In Cfg::from_env():
homes_export: std::env::var("WS_HOMES_EXPORT").ok().filter(|v| !v.is_empty()),
```

- [ ] **Step 3: Mount at startup** in `run()`, after the engine is built, before the controller starts:

```rust
/// `{pool}/homes` — where the region's shared-home export is mounted, one mount per node.
pub fn homes_root(pool: &str) -> std::path::PathBuf {
    std::path::Path::new(pool).join("homes")
}

fn mount_homes(pool: &str, export: &str) -> Result<(), String> {
    let target = homes_root(pool);
    std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;
    // Idempotent: already mounted is success. /proc/mounts is the authority.
    let mounts = std::fs::read_to_string("/proc/mounts").map_err(|e| e.to_string())?;
    if mounts.lines().any(|l| l.split_whitespace().nth(1) == target.to_str()) {
        return Ok(());
    }
    // hard,nointr: a flapping ZeroFS must block, not corrupt (spec ruling). vers=3: ZeroFS
    // serves NFSv3. nolock: no NLM sideband — append-mode files are node-local by design.
    let st = std::process::Command::new("mount")
        .args(["-t", "nfs", "-o", "vers=3,hard,nolock,tcp", export])
        .arg(&target)
        .status()
        .map_err(|e| e.to_string())?;
    if st.success() { Ok(()) } else { Err(format!("mount {export} at {} failed: {st}", target.display())) }
}
```

Call site: `if let Some(export) = &cfg.homes_export { mount_homes(&cfg.pool, export)?; }` — a failed mount fails agent startup loudly (fail closed; the DaemonSet restarts it).
- [ ] **Step 4: Unit test** (no root, no NFS — test only the /proc/mounts parse decision): extract `fn already_mounted(mounts: &str, target: &str) -> bool` from `mount_homes`, test it directly:

```rust
#[test]
fn already_mounted_matches_the_target_column_exactly() {
    let mounts = "zerofs:/ /wspool-prod/homes nfs rw 0 0\nother /wspool-prod/homes2 nfs rw 0 0\n";
    assert!(already_mounted(mounts, "/wspool-prod/homes"));
    assert!(!already_mounted(mounts, "/wspool-prod/home"));
}
```

- [ ] **Step 5: Add `WS_HOMES_EXPORT` env** to `deploy/k3s/agent-daemonset.yaml` (value `zerofs.rustic-git-system.svc:/`), with a comment pointing at zerofs.yaml. Do NOT apply to the cluster in this task.
- [ ] **Step 6: Run `cargo test --workspace --locked` and clippy; commit** `Add the region ZeroFS deployment and the agent's homes mount`.

### Task 2: Pod layout — shared home, local cache, env redirection

**Files:**
- Modify: `crates/workspaces/src/k8s.rs` (`home_volume`, `login_env`, `workspace_pod` mounts, add cache volume fns)
- Test: `crates/workspaces/src/k8s.rs` (in-file `#[cfg(test)]`, follow the existing `a_workspace_pods_host_paths_match_the_agents_layout` pattern)

**Interfaces:**
- Consumes: path convention `{pool}/homes/{owner}` (Task 1), `{pool}/homecache/{owner}` (Task 3 creates it; this task only builds mounts).
- Produces: `fn home_volume(pool: &str, owner: &str) -> Volume` (hostPath `{pool}/homes/{owner}`, type `Directory`); `fn homecache_volume(pool: &str, owner: &str) -> Volume` (name `"homecache"`, hostPath `{pool}/homecache/{owner}`, type `Directory`); `pub const HOME_CACHE_DIR: &str = "/home/kl/.local-cache"`; `pub const HOME_STATE_DIR: &str = "/home/kl/.local/state"`.

- [ ] **Step 1: Write the failing tests** (replace the two existing home-path assertions at k8s.rs:1829-1838/1873-1874 and add):

```rust
#[test]
fn the_home_is_the_shared_nfs_path_and_caches_are_local() {
    let pod = workspace_pod(&ws_spec(), "vol-1", "ws-1", &ctx(), None);
    let s = pod.spec.unwrap();
    let vols = s.volumes.unwrap();
    let path = |n: &str| vols.iter().find(|v| v.name == n).unwrap().host_path.as_ref().unwrap().path.clone();
    assert_eq!(path("home"), format!("{}/homes/{}", ctx().pool, ws_spec().owner));
    assert_eq!(path("homecache"), format!("{}/homecache/{}", ctx().pool, ws_spec().owner));
    let mounts = s.containers[0].volume_mounts.clone().unwrap();
    let sub = |mp: &str| mounts.iter().find(|m| m.mount_path == mp).map(|m| (m.name.clone(), m.sub_path.clone()));
    assert_eq!(sub(HOME_CACHE_DIR), Some(("homecache".into(), Some("cache".into()))));
    assert_eq!(sub("/home/kl/.vscode-server"), Some(("homecache".into(), Some("vscode-server".into()))));
    assert_eq!(sub("/home/kl/.cursor-server"), Some(("homecache".into(), Some("cursor-server".into()))));
    assert_eq!(sub(HOME_STATE_DIR), Some(("homecache".into(), Some("state".into()))));
}

#[test]
fn the_login_env_redirects_every_cache_and_pins_histfile_local() {
    let env = login_env("ws-1");
    let get = |n: &str| env.iter().find(|e| e.name == n).unwrap().value.clone().unwrap();
    assert_eq!(get("XDG_CACHE_HOME"), format!("{HOME_CACHE_DIR}/xdg"));
    assert_eq!(get("HISTFILE"), format!("{HOME_STATE_DIR}/shell_history"));
    for (var, sub) in [
        ("npm_config_cache", "npm"), ("PNPM_STORE_DIR", "pnpm"), ("BUN_INSTALL_CACHE_DIR", "bun"),
        ("CARGO_HOME", "cargo"), ("RUSTUP_HOME", "rustup"), ("GOMODCACHE", "gomod"), ("GOPATH", "go"),
        ("GRADLE_USER_HOME", "gradle"), ("UV_CACHE_DIR", "uv"), ("PIP_CACHE_DIR", "pip"),
        ("DENO_DIR", "deno"), ("PLAYWRIGHT_BROWSERS_PATH", "playwright"),
    ] {
        assert_eq!(get(var), format!("{HOME_CACHE_DIR}/{sub}"), "{var}");
    }
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test -p rustic-git-workspaces the_home_is_the_shared` — FAIL (old paths / missing vars).
- [ ] **Step 3: Implement.** `home_volume(pool, owner)` → `host_dir("home", format!("{pool}/homes/{owner}"))`; delete the `home_volume_name`-based call at k8s.rs:886 and pass `&spec.owner`. Add `homecache_volume` and the four subPath mounts (subPaths `cache`, `vscode-server`, `cursor-server`, `state` — subPath so ONE volume serves all four and the janitor deletes one subvolume). Extend `login_env` with the vars above plus `HISTFILE`. Keep `ZDOTDIR` unchanged (configs stay shared; only history moves via `HISTFILE`). Apply the same mounts to the environment pod builder ONLY if it mounts a home today — it does not; touch nothing there.
- [ ] **Step 4: Run the tests — PASS. Fix any sibling tests still asserting the old `vol/home-{owner}/live/...` path (`a_workspace_pods_host_paths_match_the_agents_layout` and the two at 1829/1873).**
- [ ] **Step 5: Commit** `Mount the shared home and a local cache into workspace pods`.

### Task 3: Agent provisions the home dir and cache subvolume; HomeNotReady re-gated

**Files:**
- Modify: `bins/agent/src/controller.rs:2140-2180` (the HomeNotReady gate)
- Modify: `crates/workspaces/src/engine/ops.rs` (new `ensure_homecache`)
- Test: `bins/agent/tests/reconcile.rs`; engine half via `crates/workspaces/tests/engine_ops.rs` (btrfs-gated, run on build-0)

**Interfaces:**
- Consumes: `homes_root(pool)` (Task 1), path `{pool}/homecache/{owner}`.
- Produces: `Engine::ensure_homecache(&self, owner: &str, uid: u32) -> Result<(), EngErr>` (creates `{pool}/homecache/{owner}` as a btrfs subvolume plus `cache`/`vscode-server`/`cursor-server`/`state` dirs, chowned to uid); `fn ensure_shared_home(pool: &str, owner: &str, uid: u32) -> Result<(), String>` in controller.rs (mkdir + chown `{pool}/homes/{owner}` — plain fs ops, it is NFS).

- [ ] **Step 1: Engine test** (append to `engine_ops.rs`, gated like its siblings):

```rust
#[test]
fn ensure_homecache_creates_a_subvolume_with_the_four_dirs_owned_by_the_uid() {
    let (engine, _tmp) = engine(); // the file's existing btrfs-pool fixture
    engine.ensure_homecache("alice", 1000).unwrap();
    let root = engine.pool.root.join("homecache/alice");
    assert!(crate::is_subvolume(&root));
    for d in ["cache", "vscode-server", "cursor-server", "state"] {
        let m = std::fs::metadata(root.join(d)).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(m.uid(), 1000, "{d}");
    }
    engine.ensure_homecache("alice", 1000).unwrap(); // idempotent
}
```

- [ ] **Step 2: Implement `ensure_homecache`** in ops.rs: `create_dir_all` the parent, `btrfs subvolume create` if absent (is_subvolume check first — an existing plain dir from a crash is converted by move-aside is NOT needed: creation order makes the subvolume first; just error if a non-subvolume exists), create the four dirs, `chown` all to `(uid, uid)` via `std::os::unix::fs::chown`. No qgroup limit — disposable by contract (spec).
- [ ] **Step 3: Replace the HomeNotReady gate** (controller.rs:2140-2180). Delete the home-`Volume` lookup entirely; in its place, before the pod is applied:

```rust
// The shared home replaces the home Volume (spec 2026-09-01): the agent makes the two mount
// sources exist before kubelet needs them. `{pool}/homes/{owner}` is NFS — mkdir is the whole
// materialize. The cache subvolume is local and disposable. Both idempotent, so every reconcile
// may call them. No WS_HOMES_EXPORT on this node: park, fail closed — a pod started anyway
// would hostPath an empty local dir and the person's dotfiles would silently not be theirs.
if ctx.homes_export.is_none() {
    let st = crd::WorkspaceStatus {
        phase: crd::Phase::Creating,
        observed_generation: None,
        volume_ref: Some(id),
        conditions: ws_conditions(&prev, crd::condition("Ready", false, "HomeNotReady", "this node has no shared-home mount (WS_HOMES_EXPORT)", gen)),
        ..prev
    };
    write_ws_status(w, st, ctx).await?;
    return Ok(Action::requeue(TICK));
}
ensure_shared_home(&ctx.pool, &w.spec.owner, k8s::SSH_UID as u32).map_err(ReconcileErr)?;
let (engine, owner) = (ctx.engine.clone(), w.spec.owner.clone());
tokio::task::spawn_blocking(move || engine.ensure_homecache(&owner, k8s::SSH_UID as u32))
    .await.map_err(|e| ReconcileErr(e.to_string()))?.map_err(|e| ReconcileErr(e.0))?;
```

`Ctx` gains `pub homes_export: Option<String>` (threaded from `Cfg` in `Ctx::new`; every existing `Ctx::new` call site and test fixture gains the parameter — test fixtures pass `Some("test:/".into())`).
- [ ] **Step 4: Reconcile test** (mock-client, no btrfs — `ensure_shared_home` on a tmpdir works because it is plain fs ops; `ensure_homecache` will fail without btrfs, so for THIS test point the pool at a tmpdir and assert the parked path only):

```rust
#[tokio::test]
async fn a_node_without_a_homes_export_parks_the_workspace_instead_of_starting_a_pod() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx_without_homes_export(tmp.path(), vec![patch_ok(WS_STATUS)]); // fixture variant
    let w = placed_ws(); // existing fixture: status.nodeName == this node
    let action = rustic_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)));
    let st = rec.sent("PATCH", WS_STATUS);
    assert_eq!(st.last().unwrap()["status"]["conditions"][0]["reason"], "HomeNotReady");
    assert!(rec.calls().iter().all(|c| !c.contains("/pods")), "no pod while unmounted: {:?}", rec.calls());
}
```

(Adapt fixture names to what `reconcile.rs` actually exports — read its `ctx()` helper first.)
- [ ] **Step 5: Run agent tests locally; run `engine_ops` on build-0 (20.219.21.58): rsync tree, `cargo test --no-run -p rustic-git-workspaces --test engine_ops`, `sudo <binary> ensure_homecache`. Both green.**
- [ ] **Step 6: Commit** `Provision the shared home and local cache from the workspace reconcile`.

### Task 4: Stop path — drop the home-push gate

**Files:**
- Modify: `bins/agent/src/controller.rs` (`stop_workspace`, 1895-1975)
- Test: `bins/agent/tests/reconcile.rs` (existing stop tests around `ws_stop_routes`)

**Interfaces:**
- Consumes: nothing new. Produces: `stop_workspace` with no home logic — later tasks delete what it stopped calling.

- [ ] **Step 1: Rewrite `stop_workspace`**: delete the `home_here` block, the `stop-home-{id}` request, the `stop_push` call and the post-delete `delete_ignoring_404` of the request. The NFS home is durable at all times — there is nothing to push before the pod goes. Keep: the already-stopped fast path, the pod delete by `w.name_any()` (NOT `id` — the clone-kills-source bug), the final status write. Update the doc comment: "Stop the workspace: delete the pod. The home is on the shared NFS mount and needs no push (spec 2026-09-01)."
- [ ] **Step 2: Fix the stop tests**: `ws_stop_routes` drops its `WS_STOP_REQ` route (and the `Some(...)`/`None` parameter — every stop is now gateless); delete tests that assert the push-before-stop wait; keep/adjust the test asserting the pod is deleted and phase goes `Stopped`. Add:

```rust
#[tokio::test]
async fn a_stop_deletes_the_pod_without_any_home_push_gate() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![
        Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        Route { method: "DELETE", path: WS_POD_DEL.into(), status: 200, body: serde_json::json!({"kind": "Status"}) },
    ]);
    let w = stopping_ws();
    rustic_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {WS_POD_DEL}")));
    assert!(rec.calls().iter().all(|c| !c.contains("snapshots/stop-home")), "no stop-home gate: {:?}", rec.calls());
}
```

- [ ] **Step 3: Tests + clippy green; commit** `Stop a workspace without gating on a home push`.

### Task 5: Delete the home push and commit beats

**Files:**
- Modify: `bins/agent/src/controller.rs` (`spawn_home_push`, `homes_to_push`, `home_push_interval`, `home_commit_beat*`, `migrate_and_seed_baseline` call chain)
- Modify: `bins/agent/src/snapshot.rs` (the `kind == "Home"` arm, `newest_ready_commit`'s home-pin in `worktree_heads` at 215-227, the `Home` arm of `worktree_node`)
- Modify: `crates/workspaces/src/engine/ops.rs`/`pool.rs` (`pushed_generation`, `record_pushed_gen`, `pushed_gen`, `.pushed-gen` handling) — delete if no non-home caller remains (grep first)
- Test: existing tests of those fns are deleted with them; count before/after

**Interfaces:** none produced; Task 6 consumes the now-dead `home_target` plumbing.

- [ ] **Step 1: Record the test COUNT** of `cargo test -p rustic-git-agent-bin --all-targets` (the deleted-8-tests lesson: compare counts, not colors).
- [ ] **Step 2: Delete** `spawn_home_push` + its call at controller.rs:256, `home_push_interval`, `homes_to_push`, `home_commit_beat`, `home_commit_beat_one`, `migrate_and_seed_baseline` (home-layout migration — moot, homes leave btrfs). In snapshot.rs: `worktree_node`'s `("Home", ...)` arm (a Snapshot naming a home volume can no longer resolve — returns `Ok(None)`, requeue, and after Task 6 no such Snapshot is created), the `kind == "Home"` branch of `reconcile_commit`, the home-pin block in `worktree_heads`. Then chase `never used` warnings iteratively (`pushed_generation`, `record_pushed_gen`, `pushed_gen`, `WS_HOME_PUSH_SECS` docs) until clippy is silent — same rmdead sweep as the replicate_beat deletion.
- [ ] **Step 3: Delete their tests; diff the count from Step 1 — every disappearance must be a home-beat test you can name.** Run workspace tests + clippy.
- [ ] **Step 4: Commit** `Delete the home push and commit beats`.

### Task 6: Delete home materialize and the volume-side special cases

**Files:**
- Modify: `bins/agent/src/controller.rs` (`apply_volume`/`volume_work`: `home`, `home_target`, `HomeAwaitingSync` requeue at 819, `is_home_volume` filters at 505/578/852, `ensure_home_dirs` call at 1013)
- Modify: `crates/workspaces/src/engine/ops.rs` (`ensure_home_dirs`, `HOME_AWAITING_SYNC`, `materialize_home` if separate)
- Modify: `bins/agent/src/claim.rs` (any home special-case — grep `home` first)
- Test: `bins/agent/tests/reconcile.rs` home-materialize tests deleted; count as in Task 5

- [ ] **Step 1: Record test counts (agent + workspaces).**
- [ ] **Step 2: In `volume_work`**: delete the `home` and `home_target` fields of `Work` and the whole `if home { match home_target {...} }` arm — a Volume reconcile no longer has a home case at all (`is_home_volume` volumes stop being created in Task 7; existing ones are deleted in Task 9, and until then their reconcile takes the ordinary path, which is safe: `live` exists, materialize no-ops, quota applies — acceptable for the cutover window). Delete the `HOME_AWAITING_SYNC` requeue arm at 819 and the `ensure_home_dirs` call at 1013.
- [ ] **Step 3: Delete `ensure_home_dirs` and `HOME_AWAITING_SYNC`** from ops.rs (grep for remaining callers first — snapshot.rs's were removed in Task 5). Sweep `never used` to silence.
- [ ] **Step 4: Delete the dead tests, reconcile counts, run everything + clippy. Engine tests on build-0 (ops.rs changed).**
- [ ] **Step 5: Commit** `Delete home materialize and the volume reconciler's home cases`.

### Task 7: De-pin the OwnerBinding

**Files:**
- Modify: `bins/agent/src/claim.rs` (`bound_elsewhere` deleted; `ensure_binding` keeps stamping `node_name` — write-only now)
- Modify: `bins/agent/src/binding.rs` (`ensure_home` deleted; header comment rewritten)
- Modify: `bins/agent/src/controller.rs:330,383` (bindings watch: `mine.clone()` → `watcher::Config::default()` — every node reconciles every binding)
- Modify: `crates/workspaces/src/crd.rs` (doc comments on `OwnerBindingSpec.node_name`/`home_quota_gb`: retained for parse-compat, read by nothing)
- Modify: `deploy/k3s/agent-admission.yaml` (drop the `quotaGb`-on-OwnerBinding-child exception — no agent writes it any more)
- Test: `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Produces: `claim()` without the `bound_elsewhere` call — placement is `may_claim` alone (Synced replica or bootstrap). Nothing else changes in the claim.

- [ ] **Step 1: Failing test** — the claim no longer defers to a binding on another node:

```rust
#[tokio::test]
async fn a_binding_on_another_node_no_longer_blocks_a_claim() {
    let tmp = tempfile::tempdir().unwrap();
    // Fixture: unplaced workspace, this node in compatibleNodes, an OwnerBinding whose
    // spec.nodeName is a DIFFERENT node, and a 200 for the status write + binding create 409.
    // Assert the claim WRITES status.nodeName == this node instead of await_change'ing.
    // (Build the routes from claim.rs's existing claim-test fixtures — read them first.)
}
```

Flesh the fixture from the existing claim tests in `reconcile.rs` (grep `claim_workspace`); the assertion is `rec.sent("PUT", WS_STATUS)` non-empty with `nodeName` == the test node.
- [ ] **Step 2: Run — FAIL (claim defers). Delete `bound_elsewhere` and its call; run — PASS.**
- [ ] **Step 3: Binding on every node**: change the bindings controller's watch config from `mine_bindings` to `watcher::Config::default()`. AUDIT `apply_binding` for convergence under two concurrent reconcilers: `ensure(...)` is create-or-server-side-apply (verify by reading `ensure`); `write_binding_status` must tolerate 409 (it PATCHes status — confirm; if it PUTs with resourceVersion, make lost races a requeue, not an error). Delete `ensure_home(b, ctx)` call and the fn. Rewrite the module doc: the binding is the owner's namespace ensurer; `node_name` is a historical field nothing reads.
- [ ] **Step 4: Update `agent-admission.yaml`**: the Volume validation drops the `quotaGb`/OwnerBinding exception — expression becomes `restoreTo`-only. (Applied to the cluster at rollout, Task 9.)
- [ ] **Step 5: crd.rs doc-comment updates + `CRD_REGEN=1 cargo test -p rustic-git-workspaces --test crd_yaml`. Full tests + clippy. Commit** `Retire the owner-to-node pin; bindings ensure namespaces from every node`.

### Task 8: API and CRD surface cleanup

**Files:**
- Modify: `crates/workspaces/src/api.rs:1778-1786` (phantom home row in the volumes listing)
- Modify: `crates/workspaces/src/crd.rs` (`home_volume_name`, `is_home_volume`, `DEFAULT_HOME_QUOTA_GB` — delete if caller-free after Tasks 5-7; grep first, keep any with a live caller and note why)
- Modify: `deploy/k3s/agent-rbac.yaml` (comment table only, if it names homes)
- Modify: `CLAUDE.md` ("Every person has one persistent home per node" paragraph → the shared-home story; also fix the stale local-PV attach description while in there)
- Test: `crates/workspaces/tests/api_volumes.rs` or the listing's existing test

- [ ] **Step 1: Delete the `live.insert(crd::home_volume_name(owner), ...)` injection** at api.rs:1782; fix/extend the listing test to assert no `Home` row appears.
- [ ] **Step 2: Grep-and-delete caller-free home helpers in crd.rs; sweep warnings.** `is_home_volume` likely retains one caller until Task 9's data deletion — if so keep it with a `// deleted with the last home Volume (Task 9)` note.
- [ ] **Step 3: Rewrite the CLAUDE.md home paragraph** (shared NFS home per region via ZeroFS, local `.local-cache`, no home Volume, no pin) and the attach-PV sentence (hostPath since the PV deletion).
- [ ] **Step 4: Full tests + clippy. Commit** `Remove the home volume from the API surface and the docs`.

### Task 9: Rollout and migration (operator-run, documented not coded)

**Files:**
- Modify: `deploy/k3s/README.md` (new "Shared home" section + release runbook)
- Modify: `tests/ws_e2e.sh` (ZeroFS prerequisite → exit 77 without it; drop home-push assertions)

- [ ] **Step 1: Write the runbook** in README.md, this order: (1) create the `zerofs-store` Secret, `kubectl apply -f zerofs.yaml`, wait Ready; (2) add `WS_HOMES_EXPORT` to agent-daemonset.yaml, roll agents (they mount on start); (3) **migrate**: for each owner with a home Volume — stop their pods; on the binding's node `rsync -a --exclude='.cache' --exclude='.npm' --exclude='.cargo/registry' --exclude='.local/share/pnpm' --exclude='.vscode-server' --exclude='.cursor-server' /wspool-prod/vol/home-{owner}/live/home-{owner}/ /wspool-prod/homes/{owner}/` (trailing slashes matter); restart; (4) apply the new `agent-admission.yaml`; (5) **days later, irreversible**: delete each `home-{owner}` Volume CR (finalizer cleans the subvolume) and remove `is_home_volume` if it was retained. Mark step 5 with the same days-gated warning the registry-blob cleanup section uses.
- [ ] **Step 2: Update `ws_e2e.sh`**: prerequisite check `kubectl -n rustic-git-system get deploy zerofs` (missing → `exit 77`); delete home-push wait sections; add one assertion — write a file in `~/` in workspace A, stop A, start a workspace on the OTHER node (now legal), read the file back over NFS.
- [ ] **Step 3: Commit** `Document the shared-home rollout and migration`.

### Task 10: Cluster verification (the payoff)

No files — live checks after the operator runs Task 9's runbook. Record results in the PR/commit message of a final fixes commit if anything needs fixing.

- [ ] **Step 1:** `deploy/pin.sh <sha>`, roll, all pods Running, both agents log the homes mount.
- [ ] **Step 2:** Existing workspaces still Running, dotfiles present over NFS (`cat ~/.gitconfig` in a pod).
- [ ] **Step 3:** Clone a workspace and verify it can now claim EITHER node (repeat until it lands off `session-0`, or cordon `session-0` briefly) — pod Running on the other node, same `~/.gitconfig` visible, commits pull as before.
- [ ] **Step 4:** Dead-node auto-heal end to end (spec open item): stop kubelet on one node (or scale a test to it and cordon+drain), wait `WS_NODE_DEAD_SECS`, verify `unclaim_dead_nodes` un-places and the survivor claims it — the workspace comes back with its home intact. This is the behaviour the whole change buys; it must be observed, not assumed.
- [ ] **Step 5:** Benchmark gates on build-0 or the cluster (spec open item): warm/cold `npm install` in a pod (cache redirected — verify it lands in `.local-cache/npm`), `ls -la ~` latency, VS Code server start. Record numbers in the ledger; regressions beyond "cold cache re-downloads" are stop-ship.

## Self-review

- Spec coverage: layout ✓ (T2/T3), ZeroFS deployment ✓ (T1), provisioning order ✓ (T3), binding de-pin + convergence audit ✓ (T7), deletion order ✓ (T4→T5→T6 numbered), migration ✓ (T9), size alarm — **gap**: added here → fold into Task 9 Step 1's runbook as a janitor TODO is a placeholder; instead: the janitor change is small, add to Task 3 Step 2: `ensure_homecache` is engine-side; the alarm belongs in `bins/agent/src/janitor.rs` — one `du -s`-equivalent walk per sweep over `{pool}/homes/*` logging `warn!` above 100 MB. Added as Task 3 Step 2a: implement `warn_oversized_homes(pool)` in janitor.rs with a unit test on a tmpdir (plain fs, no btrfs).
- Placeholders: Task 7 Step 1's fixture is deliberately sketched with instructions to read the existing claim fixtures — acceptable: the exact routes depend on fixture helpers the implementer must read anyway; the assertion is fully specified.
- Type consistency: `homes_root(pool)`, `ensure_homecache(owner, uid)`, `ensure_shared_home(pool, owner, uid)`, `HOME_CACHE_DIR`, `HOME_STATE_DIR`, `homecache_volume(pool, owner)` used consistently across T1-T3.
