# Home Is the Workspace Volume Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Mount each workspace's btrfs volume as the whole `/home/kl`, with the source tree at `~/workspace`, and remove the shared NFS home, the node-local homecache and the per-name workspaces tree.

**Architecture:** The subvolume path and every snapshot/sync/clone/restore mechanism are unchanged; only what is inside the subvolume and where the pod mounts it change. Existing trees are not migrated (owner ruling 23 Sep). The bench gets its own Volume instead of an NFS folder. The NFS mount, `WS_HOMES_EXPORT` and `hostPID` go now.

**Tech Stack:** Rust (kube-rs controllers in `bins/agent`, pod builders in `crates/workspaces/src/k8s`), btrfs, bash probe stages in `bins/slo`.

**Spec:** `docs/superpowers/specs/2026-09-22-home-is-the-workspace-volume-design.md`

## Global Constraints

- Subvolume path `Engine::worktree(volume, ws)` is unchanged; no CRD schema change except the new bench Volume name `bench-{id}`.
- Mounted at `/home/kl` (`k8s::HOME_DIR`); source at `/home/kl/workspace` (`k8s::WORKSPACE_DIR`). No layout marker.
- Nothing the platform mints lives in the volume: `authorized_keys`, `resolv.conf`, `user-key` Secret, `/tmp` (emptyDir), nix store stay mounts.
- No migration: existing workspaces are mounted as they are (owner ruling 23 Sep 2026).
- `mount_homes`, `homes_export`, `WS_HOMES_EXPORT`, `HomeNotReady` and the agent DaemonSet `hostPID` are deleted in this ship.
- Build/test/clippy run in the dev pod or with `CARGO_TARGET_DIR=/Volumes/kdisk/target-home-volume`; `cargo clippy --workspace --all-targets -- -D warnings` must pass before every commit.
- Commit subjects imperative sentence case, no tool attribution, no Co-Authored-By.
- `deploy/slo.md` must equal the catalogue (a test enforces it).
- Comments explain WHY; module docs carry context; files stay under ~800 lines.

---

### Task 1: Constants and the pod shape

**Files:**
- Modify: `crates/workspaces/src/k8s/mod.rs:174-195`
- Modify: `crates/workspaces/src/k8s/workspace.rs` (`login_env`, `workspace_pod` volumes/mounts, `git_init_container`)
- Modify: `crates/workspaces/src/k8s/tests.rs`
- Modify: every caller of `workspace_dir`/`WORKSPACES_DIR`/`HOME_CACHE_DIR`/`HOME_STATE_DIR`/`SEED_DIR` (`grep -rn` across `crates/ bins/`)

**Interfaces:**
- Produces: `pub const WORKSPACE_DIR: &str = "/home/kl/workspace";` in `k8s/mod.rs`. `WORKSPACES_DIR`, `workspace_dir`, `HOME_CACHE_DIR`, `HOME_STATE_DIR`, `SEED_DIR` deleted.

- [ ] **Step 1: Write the failing test** in `k8s/tests.rs`:

```rust
#[test]
fn workspace_pod_mounts_the_volume_as_the_home() {
    let pod = fixture_pod(); // the existing workspace_pod fixture in this file
    let c = &pod.spec.as_ref().unwrap().containers[0];
    let mounts = c.volume_mounts.as_ref().unwrap();
    let live: Vec<_> = mounts.iter().filter(|m| m.name == "live").collect();
    assert_eq!(live.len(), 1, "one live mount, no subPath fan-out: {live:?}");
    assert_eq!(live[0].mount_path, HOME_DIR);
    assert!(live[0].sub_path.is_none());
    assert!(mounts.iter().all(|m| m.name != "homecache" && m.name != "workspaces"));
    assert!(mounts.iter().any(|m| m.mount_path == "/tmp"));
    let env = c.env.as_ref().unwrap();
    let names: Vec<&str> = env.iter().map(|e| e.name.as_str()).collect();
    for gone in ["XDG_CACHE_HOME", "TMPDIR", "HISTFILE", "GOCACHE", "GRADLE_USER_HOME"] {
        assert!(!names.contains(&gone), "{gone} must not be set");
    }
    let kl = env.iter().find(|e| e.name == "KL_WORKSPACE").unwrap();
    assert_eq!(kl.value.as_deref(), Some(WORKSPACE_DIR));
    let target = env.iter().find(|e| e.name == "CARGO_TARGET_DIR").unwrap();
    assert_eq!(target.value.as_deref(), Some("/home/kl/.cache/cargo-target"));
}
```

- [ ] **Step 2: Run** `cargo test -p kloudlite-workspaces workspace_pod_mounts_the_volume_as_the_home` — expect FAIL (compile: `WORKSPACE_DIR` missing).
- [ ] **Step 3: Implement.** In `k8s/mod.rs` replace lines 174-195 with `WORKSPACE_DIR` and `HOME_DIR` (kept). In `login_env` keep exactly: `CARGO_TARGET_DIR=/home/kl/.cache/cargo-target`, `RUSTUP_HOME=/home/kl/.cache/rustup`, `GOMODCACHE=/home/kl/.cache/gomod`, `MAVEN_OPTS=-Dmaven.repo.local=/home/kl/.cache/m2`, `PLAYWRIGHT_BROWSERS_PATH=/home/kl/.cache/ms-playwright`, `NUGET_PACKAGES=/home/kl/.cache/nuget`, `DO_NOT_TRACK=1`, `KL_WORKSPACE=WORKSPACE_DIR`, plus the unrelated ones already there (`MANPATH`, `XDG_DATA_DIRS`, `HOME`, `LANG`, …). Delete the rest listed in the spec. Rewrite the "Three homes" comment to one paragraph: the volume is the home, every cache is under `~/.cache` because it travels, `CARGO_TARGET_DIR` stays only so `./target` never collides with a versioned dir. In `workspace_pod`: `live` mounts once at `HOME_DIR`, no `mount_propagation`; delete the six subPath mounts, the `homecache` volume+mounts, the `workspaces` emptyDir and the per-name mount; add `Volume { name: "tmp", empty_dir }` mounted at `/tmp`. In `git_init_container`: mount `live` at `HOME_DIR`, clone into `WORKSPACE_DIR`. The `~/.cargo` root-owned chown dance (`workspace.rs:113-141`) is deleted: `~/.cargo` is an ordinary home dir now. Prelude's `cd "$KL_WORKSPACE"` line stays. Fix every other caller of the deleted symbols (`crates/ide/src/server.rs` fixture, `bins/slo/src/stages/experience_ws.rs`, `bins/agent`), replacing `workspace_dir(name)` with `WORKSPACE_DIR`.
- [ ] **Step 4: Run** `cargo test -p kloudlite-workspaces` and `cargo clippy --workspace --all-targets -- -D warnings` — expect PASS.
- [ ] **Step 5: Commit** `Mount the workspace volume as the whole home`.

---

### Task 2: (removed — owner ruling 23 Sep 2026: no migration)

---

### Task 3: Agent — drop HomeNotReady, homecache and the shared home

**Files:**
- Modify: `bins/agent/src/controller/workspace/mod.rs:207-229`
- Delete: `bins/agent/src/controller/workspace/home.rs` (`ensure_shared_home`, `ensure_bench_folder` — bench comes in Task 4; if Task 4's bench controller still calls `ensure_bench_folder`, leave that one function in place for Task 4 to delete and delete only `ensure_shared_home`)
- Modify: `bins/agent/src/lib.rs:45-64,154,249-250` (delete `mount_homes`, `homes_export`, and the `WS_HOMES_EXPORT` read)
- Modify: `bins/agent/src/controller/mod.rs:238-241,306,369`, `bins/agent/src/stats.rs:109`, `bins/agent/src/main.rs:37`, `bins/agent/src/claim.rs:423-427` (remove the homes gate)
- Modify: `crates/workspaces/src/engine/ops.rs:113-132` (delete `ensure_homecache`), `bins/agent/src/janitor.rs:189`, `bins/agent/src/controller/environment/mod.rs:98`
- Modify: `bins/agent/tests/reconcile/*` (`ctx_with_homes_export`/`ctx_without_homes_export` → one `ctx()`; delete the HomeNotReady test; add the test below)

- [ ] **Step 1: Write failing reconcile test** (in the existing reconcile harness, same fixtures as the `Creating` tests): a workspace reconciled with no homes export configured reaches the pod-write step and its status reason is never `HomeNotReady`. Use the existing helpers; name it `a_workspace_starts_without_a_homes_export`.
- [ ] **Step 2: Run** it — FAIL (`HomeNotReady`).
- [ ] **Step 3: Implement.** In `apply_workspace` delete the `homes_export` gate and the `ensure_shared_home`/`ensure_homecache` block. Delete `home.rs` (see Files), `ensure_homecache`, the janitor homecache sweep, the environment call, the stats reason, the claim gate, `mount_homes`, `homes_export` and every `WS_HOMES_EXPORT` read in Rust.
- [ ] **Step 4: Run** `cargo test -p kloudlite-agent` + clippy — PASS.
- [ ] **Step 5: Commit** `Start workspaces from their own home`.

---

### Task 4: Bench on its own Volume

**Files:**
- Modify: `bins/agent/src/controller/bench.rs:170-200`
- Modify: `crates/workspaces/src/k8s/bench.rs:12,25,100-130`
- Modify: `crates/workspaces/src/quota.rs` (count `bench-{id}` volumes' disk the way it counts any Volume — verify it already does by owner label; if so no change)
- Test: `crates/workspaces/src/k8s/tests.rs`, `bins/agent/tests/reconcile/bench.rs`

**Interfaces:**
- Consumes: the workspace controller's Volume create/claim helpers (`ensure_volume`, `take_volume` in `bins/agent/src/controller/workspace/`) — reuse, do not copy.
- Produces: Volume name `format!("bench-{id}")`, `spec.replicas: 1`, ownerReference the Bench.

- [ ] **Step 1: Failing tests:** pod test — `bench_pod` has exactly one hostPath `live` at `HOME_DIR`, no `bench-folder`, no `home`, `tmp` emptyDir at `/tmp`, env `KL_BENCH_DIR=/home/kl/bench`. Reconcile test — reconciling a Bench creates Volume `bench-{id}` with `replicas: 1` and an ownerReference to the Bench, and the pod is written only after the volume is claimed.
- [ ] **Step 2: Run** — FAIL.
- [ ] **Step 3: Implement.** `bench_pod` takes the worktree path like `workspace_pod`; `BENCH_DIR` becomes `"/home/kl/bench"`; the pod's command runs `mkdir -p "$KL_BENCH_DIR"` before `harness-bench` (the harness reads `KL_BENCH_DIR`, update `harness/bench/src` where `/bench` is hardcoded — grep). Controller: replace the `homes_export`/`ensure_bench_folder` block with `ensure_volume(bench-{id}) → take_volume → write pod`. `FolderNotReady` becomes `VolumeNotReady`. Delete `ensure_bench_folder`.
- [ ] **Step 4: Run** tests + clippy — PASS.
- [ ] **Step 5: Commit** `Give each bench its own volume`.

---

### Task 5: Deploy files, policy, e2e script, docs

**Files:**
- Modify: `deploy/k3s/workspace-admission.yaml:30,71,88-93` (drop `homes/`, `homecache/` from the hostPath allow-list; keep `keys/`, `attach/`, `vol/`)
- Modify: `deploy/k3s/agent-daemonset.yaml:157-164` (delete the Azure Files / `WS_HOMES_EXPORT` env and `hostPID: true`; if any other part of the DaemonSet still needs `hostPID`, keep it and say why in the report)
- Modify: `tests/ws_e2e.sh:328,342` (drop `WS_HOMES_EXPORT`; assert `~/workspace` exists after a create)
- Modify: `deploy/k3s/README.md:146,427`, `docs/product/**/*.md` (`/home/kl/workspaces/{name}` → `/home/kl/workspace`; add one limits line: "a clone or restore carries the whole home, credentials included; it is always your own workspace")
- Modify: `harness/bench/src/sub.ts:76-77`, `docs/superpowers/plans/2026-09-22-sys1-sessions.md:1068,1071,1235,1470`, `docs/superpowers/specs/2026-09-22-sys1-sessions-design.md:134` (clone-push path `/home/kl/workspace`; "the workspace's home")
- Modify: `crates/workspaces/src/model.rs:278` doc, `CLAUDE.md` "Every person has one persistent home per region" paragraph → rewrite to the new rule (four sentences: volume is the home, `~/workspace`, existing trees are not migrated, mounts that stay)

- [ ] **Step 1:** `grep -rn "workspaces/\|homecache\|HomeNotReady\|shared NFS\|home.persists" deploy docs tests harness CLAUDE.md crates/workspaces/src/model.rs` and fix every hit per the spec.
- [ ] **Step 2:** `kubectl --dry-run=client apply -f deploy/k3s/workspace-admission.yaml -f deploy/k3s/agent-daemonset.yaml` — expect valid.
- [ ] **Step 3: Commit** `Describe the home layout in deploy files and docs`.

---

### Task 6: Probes

**Files:**
- Modify: `crates/workspaces/src/slo/catalogue.rs:213,390,421`, `deploy/slo.md:151,180`
- Modify: `bins/slo/src/stages/experience_ws.rs:266-311,463,484,584-587,858,921,929-954`

- [ ] **Step 1:** Catalogue: delete `homecache.not_subvolume`; `home.persists` → `Slo { id: "home.travels", feature: "Workspaces", sli: "A dotfile and a file under ~/workspace written before a push are present in a workspace restored from it, and absent from a fresh workspace of the same owner", target: avail(99.9), suite: Suite::Hourly, stage: "14 · Experience" }`; `ws.cache.travels` sli text `~/.cache` instead of `{ws}/.cache`. Mirror `deploy/slo.md`. Run `cargo test -p kloudlite-workspaces slo` — the equality test must PASS.
- [ ] **Step 2:** Stage: rewrite the `home.persists` block as `home.travels` per the sli (write `~/.config/kl-probe` and `~/workspace/probe.txt` in A via the tool server `write`; push; restore to B; `read` both; create C; `read` `~/.config/kl-probe` must 404). Replace every `workspace_dir(name)` with `WORKSPACE_DIR` and `{ws}/.cache` with `/home/kl/.cache`.
- [ ] **Step 3:** `cargo test -p kloudlite-slo` + clippy — PASS.
- [ ] **Step 4: Commit** `Probe that the home travels with the workspace`.

---

### Task 7: Fleet verification (owner-run, not a subagent)

- [ ] Build in the dev pod, `deploy/dev/dev-push.sh --aks`, apply `workspace-admission.yaml` and `agent-daemonset.yaml` on k3s, roll agent + api.
- [ ] Create a fresh workspace with `repo`: `~/workspace` holds the clone, no `HomeNotReady` ever.
- [ ] Start a bench: Volume `bench-{id}` exists, pod runs, `~/bench` writable.
- [ ] Run the hourly suite: `home.travels`, `ws.cache.travels`, `ide.*`, `bench.*` pass.
- [ ] Retire the Azure Files shares by hand.
