# Volume Takeover Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A volume owned by a dead node becomes `Unavailable`; a STOPPED workspace or environment on that node is released and re-hosted by a Synced survivor, a RUNNING one is left pinned and marked `NodeDead` until the person stops it.

**Architecture:** Three arms on existing paths: the dead-node sweep in `bins/agent/src/peer.rs` clears `Volume.spec.nodeName`; `resolve_volume` in `bins/agent/src/controller.rs` takes an empty pin with a JSON-patch `test` compare-and-set; the pull beat on a non-owner deletes stale `live/` worktrees. The admission policy allows exactly the two `nodeName` transitions.

**Tech Stack:** Rust (kube-rs, k8s-openapi), CEL admission policy, btrfs.

**Spec:** `docs/superpowers/specs/2026-09-02-volume-takeover-design.md`

## Global Constraints

- The agent writes spec on a Volume ONLY for `restoreTo` and `nodeName`; `nodeName` may change only `owned -> ""` (sweep) or `"" -> me` (takeover). Never `owned -> other`.
- Takeover uses a JSON patch whose first op is `{"op":"test","path":"/spec/nodeName","value":""}`. No read-modify-write.
- Every sweep and takeover is keep-biased: a list error clears nothing; a patch error takes nothing.
- The sweep NEVER un-places a parent whose `spec.desiredState` is `Running`, and never clears a Volume pin while any parent naming that volume is `Running`.
- CI gate: `cargo clippy --workspace --all-targets --locked -- -D warnings`, and `cargo test -p rustic-git-agent -p rustic-git-workspaces`.
- Commit subjects imperative sentence case, no tool attribution, no trailers.
- Comments say WHY, never what.

---

### Task 1: `Phase::Unavailable` and the admission rule

**Files:**
- Modify: `crates/workspaces/src/crd.rs` (the `Phase` enum, near line 393)
- Modify: `deploy/k3s/agent-admission.yaml` (the `object.kind == 'Volume'` expression)
- Modify: `deploy/k3s/agent-rbac.yaml` header table (one line: `nodeName` is the second spec exception)

**Interfaces:**
- Produces: `crd::Phase::Unavailable` (serializes as `"Unavailable"`).

- [ ] **Step 1: Add the variant**

```rust
    Working,
    /// The owning node is dead and the pin has been cleared: no node may write this subvolume
    /// until one takes it (`resolve_volume`'s takeover arm). Distinct from `Error` so an
    /// operator can tell "waiting for a Synced survivor" from "something is broken".
    Unavailable,
```

- [ ] **Step 2: Extend the admission expression**

Replace the Volume branch with:

```yaml
        object.kind == 'Volume'
          ? (object.spec.all(k, k == 'restoreTo'
                                || (k == 'nodeName' && (oldObject.spec.nodeName == '' || object.spec.nodeName == ''))
                                || (k in oldObject.spec && object.spec[k] == oldObject.spec[k]))
             && oldObject.spec.all(k, k == 'restoreTo' || k in object.spec))
          : object.spec == oldObject.spec
      message: "rustic-git-agent writes status, not spec (exceptions: Volume.spec.restoreTo, and Volume.spec.nodeName only owned->'' or ''->node)"
```

- [ ] **Step 3: Verify the CRD still generates and tests pass**

Run: `cargo test -p rustic-git-workspaces`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/workspaces/src/crd.rs deploy/k3s/agent-admission.yaml deploy/k3s/agent-rbac.yaml
git commit -m "Add the Unavailable volume phase and allow the two nodeName transitions"
```

### Task 2: The sweep spares Running worktrees and releases stopped ones' volumes

**Files:**
- Modify: `bins/agent/src/peer.rs` (`unclaim_dead_nodes` ~line 700, `unclaim_kind` ~line 733; tests near line 1181)
- Modify: `crates/workspaces/src/crd.rs` (`VolumeStatus.conditions` if absent)

**Interfaces:**
- Consumes: `unclaim_kind`, `node_is_dead`, `replace_status`, `crd::Phase::Unavailable`, `crd::DesiredState`.
- Produces: `unclaim_kind` gains a `releasable: impl Fn(&K) -> bool` argument; `async fn release_dead_volumes(ctx, nodes, floor, now, running_volumes: &HashSet<String>)`.

- [ ] **Step 1: Write the failing tests**

Beside the existing unclaim tests (same harness as the one at ~line 1224). Add helpers `ws_placed_stopped(name, node)` (copy `ws_placed`, `desiredState: "stopped"`, plus `"volumeRef": {"name": format!("vol-{name}")}` in status — match the real field name with `grep -n volume_ref crates/workspaces/src/crd.rs`) and `vol_owned(name, node)`; give `ws_placed` the same `volumeRef`. Add `VOLUMES` path constant and, if the recorder lacks it, `body_of(call)`.

```rust
    #[tokio::test]
    async fn a_running_worktree_on_a_dead_node_is_marked_not_moved() {
        let rec = Recorder::default();
        let ctx = ctx_with_routes(&rec, vec![
            Route { method: "GET", path: NODES.into(), status: 200, body: list_of("Node", vec![node_ready("node-a"), node_dead("node-b")]) },
            Route { method: "GET", path: WORKSPACES.into(), status: 200, body: list_of("Workspace", vec![ws_placed("ws-run", "node-b")]) },
            Route { method: "GET", path: ENVIRONMENTS.into(), status: 200, body: list_of("Environment", vec![]) },
            Route { method: "GET", path: VOLUMES.into(), status: 200, body: list_of("Volume", vec![vol_owned("vol-ws-run", "node-b")]) },
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/workspaces/ws-run/status".into(), status: 200, body: ws_placed("ws-run", "node-b") },
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/volumes/vol-ws-run/status".into(), status: 200, body: vol_owned("vol-ws-run", "node-b") },
        ]);
        unclaim_dead_nodes(&ctx).await;
        let ws = rec.body_of("PUT /apis/rustic-git.io/v1alpha1/workspaces/ws-run/status");
        assert_eq!(ws["status"]["nodeName"], "node-b", "a running worktree keeps its node");
        assert_eq!(ws["status"]["conditions"][0]["reason"], "NodeDead");
        assert!(!rec.calls().iter().any(|c| c == "PATCH /apis/rustic-git.io/v1alpha1/volumes/vol-ws-run"), "pin untouched");
        let vol = rec.body_of("PUT /apis/rustic-git.io/v1alpha1/volumes/vol-ws-run/status");
        assert_eq!(vol["status"]["phase"], "Unavailable");
    }

    #[tokio::test]
    async fn a_stopped_worktree_on_a_dead_node_is_released_with_its_volume() {
        let rec = Recorder::default();
        let ctx = ctx_with_routes(&rec, vec![
            Route { method: "GET", path: NODES.into(), status: 200, body: list_of("Node", vec![node_ready("node-a"), node_dead("node-b")]) },
            Route { method: "GET", path: WORKSPACES.into(), status: 200, body: list_of("Workspace", vec![ws_placed_stopped("ws-stop", "node-b")]) },
            Route { method: "GET", path: ENVIRONMENTS.into(), status: 200, body: list_of("Environment", vec![]) },
            Route { method: "GET", path: VOLUMES.into(), status: 200, body: list_of("Volume", vec![vol_owned("vol-ws-stop", "node-b"), vol_owned("vol-live", "node-a")]) },
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/workspaces/ws-stop/status".into(), status: 200, body: ws_placed_stopped("ws-stop", "") },
            Route { method: "PATCH", path: "/apis/rustic-git.io/v1alpha1/volumes/vol-ws-stop".into(), status: 200, body: vol_owned("vol-ws-stop", "") },
            Route { method: "PUT", path: "/apis/rustic-git.io/v1alpha1/volumes/vol-ws-stop/status".into(), status: 200, body: vol_owned("vol-ws-stop", "") },
        ]);
        unclaim_dead_nodes(&ctx).await;
        let ws = rec.body_of("PUT /apis/rustic-git.io/v1alpha1/workspaces/ws-stop/status");
        assert_eq!(ws["status"]["nodeName"], "");
        assert_eq!(rec.body_of("PATCH /apis/rustic-git.io/v1alpha1/volumes/vol-ws-stop")["spec"]["nodeName"], "");
        assert_eq!(rec.body_of("PUT /apis/rustic-git.io/v1alpha1/volumes/vol-ws-stop/status")["status"]["phase"], "Unavailable");
        assert!(!rec.calls().iter().any(|c| c.contains("/volumes/vol-live")));
    }
```

- [ ] **Step 2: Run, expect failure** — `cargo test -p rustic-git-agent on_a_dead_node` — FAIL.

- [ ] **Step 3: Implement**

`unclaim_kind` gets a `releasable: impl Fn(&K) -> bool` parameter and a `degraded_status: impl Fn(&K) -> serde_json::Value`. In its loop, after the `node_is_dead` check:

```rust
        // A Running worktree is never moved by the system: its live edits exist only on the dead
        // node and only the person may decide those are expendable (spec: "the person decides").
        // It is marked so the API can say why, and released the moment desiredState is Stopped.
        let status = if releasable(&obj) { cleared_status(&obj) } else { degraded_status(&obj) };
```

and `replace_status` with that. Both callers pass `|w| w.spec.desired_state == crd::DesiredState::Stopped` (Environment the same) and a `degraded_status` that keeps `node_name` and sets `conditions = vec![crd::condition("Degraded", true, "NodeDead", &format!("node {n} is down; edits since the last sync point exist only there — stop and start to move it, or wait for the node"), gen)]` where `n` is the claimed node. Skip the write when the object already carries a `NodeDead` condition (no churn every beat).

`unclaim_dead_nodes` collects `running_volumes: HashSet<String>` — the `status.volumeRef.name` of every listed Workspace/Environment with `desiredState == Running` (have `unclaim_kind` return the listed items, or list once and pass the slices in) — then calls:

```rust
    release_dead_volumes(ctx, &nodes, floor, now, &running_volumes).await;
```

```rust
/// A dead node's Volume: phase says Unavailable for every one; the owner pin is cleared ONLY
/// when no Running parent still names it — a pinned-but-unavailable volume is exactly the
/// "down, not moved" state the spec asks for. Spec first, status second: a cleared pin with
/// stale status is taken on the next claim; the reverse is a lie the takeover arm cannot act on.
async fn release_dead_volumes(ctx: &Arc<Ctx>, nodes: &[Node], floor: i64, now: k8s_openapi::jiff::Timestamp, running: &HashSet<String>) {
    let api: Api<crd::Volume> = Api::all(ctx.client.clone());
    let list = match api.list(&ListParams::default()).await {
        Ok(l) => l.items,
        Err(e) => {
            tracing::warn!(error = %e, "pull: unclaim: listing volumes; releasing nothing");
            return;
        }
    };
    for vol in list {
        let owner = vol.spec.node_name.clone();
        if owner.is_empty() || !node_is_dead(nodes.iter().find(|n| n.name_any() == owner), floor, now) {
            continue;
        }
        let name = vol.name_any();
        let release = !running.contains(&name);
        let mut cur = vol;
        if release {
            let clear = serde_json::json!({ "spec": { "nodeName": "" } });
            if let Err(e) = api.patch(&name, &PatchParams::default(), &Patch::Merge(&clear)).await {
                tracing::warn!(volume = %name, error = %e, "pull: unclaim: releasing a dead node's volume");
                continue;
            }
            cur.spec.node_name.clear();
        }
        let mut st = cur.status.clone().unwrap_or_default();
        if st.phase == crd::Phase::Unavailable && !release {
            continue; // already marked, still pinned: nothing changed since last beat
        }
        st.phase = crd::Phase::Unavailable;
        let gen = cur.metadata.generation.unwrap_or(0);
        let why = if release { format!("owner {owner} is dead; released, waiting for a Synced node to take it") } else { format!("owner {owner} is dead; a Running worktree still names this volume, so it stays pinned") };
        st.conditions = vec![crd::condition("Available", false, "NodeDead", &why, gen)];
        if let Err(e) = replace_status(&api, &cur, "Volume", serde_json::to_value(st).expect("VolumeStatus serializes")).await {
            tracing::warn!(volume = %name, error = %e, "pull: unclaim: marking a dead node's volume unavailable");
        }
    }
}
```

If `VolumeStatus` has no `conditions`, add `#[serde(default)] pub conditions: Vec<Condition>` matching `WorkspaceStatus`'s. Import `kube::api::{Patch, PatchParams}` and `std::collections::HashSet` as needed. Existing unclaim tests that used a Running `ws_placed` and expected a cleared `nodeName` must flip to `ws_placed_stopped` — that expectation is what this task changes.

- [ ] **Step 4: Run tests** — `cargo test -p rustic-git-agent` — PASS.

- [ ] **Step 5: Commit**

```bash
git add bins/agent/src/peer.rs crates/workspaces/src/crd.rs
git commit -m "Release only stopped worktrees from a dead node and mark the rest"
```

### Task 3: Takeover in `resolve_volume`

**Files:**
- Modify: `bins/agent/src/controller.rs` (`resolve_volume`, the block ending at the `NodeMismatch` guard)

**Interfaces:**
- Consumes: `crd::Volume`, `Api::patch` with `Patch::Json`.
- Produces: `pub(crate) async fn take_volume(ctx, name, node) -> Result<bool, kube::Error>` — `Ok(true)` won, `Ok(false)` lost the race (422 on the `test` op).

- [ ] **Step 1: Write the failing tests**

In the controller test module, using its fake-API harness:

```rust
    #[tokio::test]
    async fn take_volume_wins_with_a_test_op_on_an_empty_pin() {
        let rec = Recorder::default();
        let ctx = ctx_with_routes(&rec, vec![
            Route { method: "PATCH", path: "/apis/rustic-git.io/v1alpha1/volumes/v1".into(), status: 200, body: vol_owned("v1", "node-a") },
        ]);
        assert!(take_volume(&ctx, "v1", "node-a").await.unwrap());
        let body = rec.body_of("PATCH /apis/rustic-git.io/v1alpha1/volumes/v1");
        assert_eq!(body[0], serde_json::json!({"op":"test","path":"/spec/nodeName","value":""}));
        assert_eq!(body[1], serde_json::json!({"op":"replace","path":"/spec/nodeName","value":"node-a"}));
    }

    #[tokio::test]
    async fn take_volume_loses_quietly_when_the_test_op_fails() {
        let rec = Recorder::default();
        let ctx = ctx_with_routes(&rec, vec![
            Route { method: "PATCH", path: "/apis/rustic-git.io/v1alpha1/volumes/v1".into(), status: 422, body: status_failure("test failed") },
        ]);
        assert!(!take_volume(&ctx, "v1", "node-a").await.unwrap());
    }
```

- [ ] **Step 2: Run, expect failure** — `cargo test -p rustic-git-agent take_volume` — FAIL (function missing).

- [ ] **Step 3: Implement**

```rust
/// Compare-and-set the owner pin from empty to `node`. The `test` op is what makes two claimants
/// safe: the API server applies the patch atomically, so exactly one of them sees 200 and the
/// other a 422 it treats as "lost, not broken".
pub(crate) async fn take_volume(ctx: &Arc<Ctx>, name: &str, node: &str) -> Result<bool, kube::Error> {
    let api: Api<crd::Volume> = Api::all(ctx.client.clone());
    let patch = json_patch::Patch(vec![
        json_patch::PatchOperation::Test(json_patch::TestOperation { path: "/spec/nodeName".parse().unwrap(), value: serde_json::json!("") }),
        json_patch::PatchOperation::Replace(json_patch::ReplaceOperation { path: "/spec/nodeName".parse().unwrap(), value: serde_json::json!(node) }),
    ]);
    match api.patch(name, &PatchParams::default(), &Patch::Json::<()>(patch)).await {
        Ok(_) => Ok(true),
        Err(kube::Error::Api(s)) if s.code == 422 || s.code == 409 => Ok(false),
        Err(e) => Err(e),
    }
}
```

`json_patch` is what kube-rs's `Patch::Json` takes; it is already a transitive dependency — add it to `bins/agent/Cargo.toml` at the version kube's lockfile pins (check `cargo tree -p kube-client | grep json-patch`). If `PatchOperation`'s field names differ in that version, match them; the assertions in Step 1 are on the wire shape, which is fixed by RFC 6902.

Then, in `resolve_volume`, immediately before `if vol.spec.node_name != node_name {`:

```rust
    // An unowned volume is a dead node's, released by the unclaim sweep. This node claimed the
    // parent (so `may_claim` already proved its replica is Synced): take the pin. Losing the race
    // is not an error — the next pass meets the winner's pin and the guard below refuses as usual.
    if vol.spec.node_name.is_empty() {
        if take_volume(ctx, &id, node_name).await? {
            tracing::info!(volume = %id, node = %node_name, "took over an unowned volume");
        }
        return Ok(Resolved::Wait {
            volume_ref: None,
            phase: crd::Phase::Creating,
            cond: crd::condition("Ready", false, "VolumeTakeover", "taking ownership of the released volume", gen),
            action: Action::requeue(std::time::Duration::from_secs(5)),
        });
    }
```

Then in the Volume reconciler's own pass (the arm that materializes a Volume whose `subvolume_present` is already true because it was a replica), make sure the phase moves from `Unavailable` to `Ready`: the existing `volume_is_ready` path writes `Ready` on every convergent pass — verify with `grep -n "Phase::Ready" bins/agent/src/controller.rs` that the Volume arm sets it unconditionally and does not early-return on `Unavailable`. If it does, treat `Unavailable` exactly as `Pending`.

- [ ] **Step 4: Run tests** — `cargo test -p rustic-git-agent` — PASS. Then `cargo clippy --workspace --all-targets --locked -- -D warnings`.

- [ ] **Step 5: Commit**

```bash
git add bins/agent/src/controller.rs bins/agent/Cargo.toml Cargo.lock
git commit -m "Take an unowned volume on claim with a compare-and-set on its pin"
```

### Task 4: A non-owner drops stale live worktrees

**Files:**
- Modify: `bins/agent/src/peer.rs` (`pull_volume`, the per-volume pass) — or `bins/agent/src/janitor.rs` if `pull_volume` has no engine handle; pick whichever already has `Engine` in scope.

**Interfaces:**
- Consumes: `engine.pool.live(volume)` (`crates/workspaces/src/engine/pool.rs:18`), `janitor::subvolumes_under`, `janitor::btrfs_delete` (make both `pub(crate)`).
- Produces: `pub(crate) fn drop_stale_worktrees(engine: &Engine, volume: &str, owner: &str, me: &str) -> usize`.

- [ ] **Step 1: Write the failing test** (in `janitor.rs`'s test module, which already builds a fake pool with `create_dir_all(engine.pool.voldir("v1").join("live"))`):

```rust
    #[test]
    fn stale_worktrees_go_only_when_this_node_is_not_the_owner() {
        let (engine, _tmp) = fake_engine();
        std::fs::create_dir_all(engine.pool.live("v1").join("ws-1")).unwrap();
        assert_eq!(drop_stale_worktrees(&engine, "v1", "node-b", "node-b"), 0, "owner keeps its worktrees");
        assert_eq!(drop_stale_worktrees(&engine, "v1", "", "node-b"), 0, "unowned: the takeover has not settled, keep");
        assert_eq!(drop_stale_worktrees(&engine, "v1", "node-a", "node-b"), 1);
    }
```

`btrfs_delete` on a plain directory in the test falls back to `remove_dir_all` — if it does not already, give it that fallback when `btrfs` is not on PATH (the existing tests run on a Mac).

- [ ] **Step 2: Run, expect failure** — `cargo test -p rustic-git-agent stale_worktrees` — FAIL.

- [ ] **Step 3: Implement**

```rust
/// A worktree only exists on the owner; one on any other node is what a takeover left behind.
/// An EMPTY owner is the window between release and takeover — keep everything then, because
/// the returning node may be about to take the volume back (replicas: 1).
pub(crate) fn drop_stale_worktrees(engine: &Engine, volume: &str, owner: &str, me: &str) -> usize {
    if owner.is_empty() || owner == me {
        return 0;
    }
    let mut subs = Vec::new();
    subvolumes_under(&engine.pool.live(volume), &mut subs);
    for p in &subs {
        btrfs_delete(p, volume);
    }
    subs.len()
}
```

Call it from the pull beat's per-volume pass with the Volume's `spec.node_name` and `ctx.node`, logging at info when the count is non-zero.

- [ ] **Step 4: Run tests and clippy** — PASS.

- [ ] **Step 5: Commit**

```bash
git add bins/agent/src/janitor.rs bins/agent/src/peer.rs
git commit -m "Drop a lost volume's stale worktrees on the node that lost it"
```

### Task 5: Docs and the e2e assertion

**Files:**
- Modify: `crates/workspaces/src/api.rs` (`stop_ws` ~line 900 and the environment stop handler): when the object's status carries a `NodeDead` condition, the 2xx response body includes `"warning": "node {n} is down; edits after the last sync point are only on that node and will not follow the move"` — read the node from `status.nodeName`. One test in the api test module: a stopped workspace with a `NodeDead` condition answers with that warning, one without does not.
- Modify: `CLAUDE.md` ("Workspaces and environments": one sentence after the `status.nodeName` claim sentence)
- Modify: `deploy/k3s/README.md` (a "Node death" subsection: what the sweep does, the 600 s floor, the partition caveat, and how to read `Unavailable`)
- Modify: `tests/ws_e2e.sh` (after the cross-node sync-point assertions: cordon+drain is not available in the harness, so simulate by scaling the agent DaemonSet off one node with a nodeSelector label, wait `WS_NODE_DEAD_SECS`, assert a RUNNING workspace's Volume goes `Unavailable` but keeps `nodeName` and the workspace carries `NodeDead`; then stop the workspace through the API, assert the response carries the warning, the pin clears, the workspace re-claims on the other node and its Volume `spec.nodeName` equals the new `status.nodeName`)

- [ ] **Step 1: CLAUDE.md sentence**

> When a node is dead for `WS_NODE_DEAD_SECS`, the unclaim sweep marks its volumes `Unavailable` and moves ONLY the worktrees whose `desiredState` is `Stopped` — a Running one keeps its pin and a `NodeDead` condition, because its live edits exist only on the dead node and only the person may write them off by stopping it. A released volume's pin is cleared, and the node that then claims the parent takes it with a JSON-patch `test` on the empty value (`take_volume`), the one other spec write the admission policy allows.

- [ ] **Step 2: README subsection and e2e block** as described; the e2e step is skipped (not failed) with a log line when the cluster has fewer than two agent nodes.

- [ ] **Step 3: Run** `bash -n tests/ws_e2e.sh`. Commit:

```bash
git add CLAUDE.md deploy/k3s/README.md tests/ws_e2e.sh
git commit -m "Document volume takeover and assert it end to end"
```

## Self-review

- Spec coverage: sweep with the Running/Stopped split (Task 2), takeover + guard (Task 3), returning node (Task 4), `Unavailable` + admission (Task 1), stop warning + docs/caveats (Task 5). replicas:1 return-path is Task 3's takeover arm plus Task 4's empty-owner keep rule.
- The stop handler already writes `desiredState: Stopped`; nothing new fires the move — the next sweep beat does.
- Names: `take_volume`, `release_dead_volumes`, `drop_stale_worktrees`, `Phase::Unavailable` consistent across tasks.
