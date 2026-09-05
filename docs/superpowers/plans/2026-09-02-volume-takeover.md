# Volume Takeover Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A dead node's replicas are re-created on a live third node without anyone acting; a volume owned by a dead node becomes `Unavailable`; a STOPPED workspace or environment on that node is released and re-hosted by a Synced survivor, a RUNNING one is left pinned and marked `NodeDead` until the person stops it.

**Architecture:** Four arms on existing paths: the pull beat in `bins/agent/src/peer.rs` drops dead nodes from the replica candidates so rendezvous heals onto a live one; the dead-node sweep in `bins/agent/src/peer.rs` clears `Volume.spec.nodeName`; `resolve_volume` in `bins/agent/src/controller.rs` takes an empty pin with a JSON-patch `test` compare-and-set; the pull beat on a non-owner deletes stale `live/` worktrees. The admission policy allows exactly the two `nodeName` transitions.

**Tech Stack:** Rust (kube-rs, k8s-openapi), CEL admission policy, btrfs.

**Spec:** `docs/superpowers/specs/2026-09-02-volume-takeover-design.md`

## Global Constraints

- Replica candidates for `replicate::targets` are LIVE pool nodes only; a dead owner does not count as a copy.
- A local copy is retired ONLY when this node is not owner, not target, not hosting, AND every current target reports `Synced`.
- The agent writes spec on a Volume ONLY for `restoreTo` and `nodeName`; `nodeName` may change only `owned -> ""` (sweep) or `"" -> me` (takeover). Never `owned -> other`.
- Takeover uses a JSON patch whose first op is `{"op":"test","path":"/spec/nodeName","value":""}`. No read-modify-write.
- Every sweep and takeover is keep-biased: a list error clears nothing; a patch error takes nothing.
- The sweep NEVER un-places a parent whose `spec.desiredState` is `Running`, and never clears a Volume pin while any parent naming that volume is `Running`.
- CI gate: `cargo clippy --workspace --all-targets --locked -- -D warnings`, and `cargo test -p kloudlite-agent -p kloudlite-workspaces`.
- Commit subjects imperative sentence case, no tool attribution, no trailers.
- Comments say WHY, never what.

---

### Task 0: Replica placement heals around dead nodes

**Files:**
- Modify: `bins/agent/src/peer.rs` (`pull_beat_with` ~line 301, `interesting_volumes` ~line 342, `reap_dead_replicas` ~659, `unclaim_dead_nodes` ~700; tests near 1181)

**Interfaces:**
- Consumes: `pool_nodes`, `node_is_dead`, `node_dead_secs`, `replicate::targets`.
- Produces: `fn live_nodes(pool: &[String], nodes: &[Node], floor: i64, now: Timestamp) -> Vec<String>`; `fn standby_count(owner_alive: bool, replicas: u32) -> usize`; `reap_dead_replicas` and `unclaim_dead_nodes` take `nodes: &[Node]` instead of listing themselves.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn dead_nodes_leave_the_candidate_list() {
        let now = k8s_openapi::jiff::Timestamp::now();
        let pool = vec!["node-a".to_string(), "node-b".to_string(), "node-c".to_string()];
        let nodes = vec![node_ready_obj("node-a"), node_dead_obj("node-b")]; // node-c: no Node object at all
        assert_eq!(live_nodes(&pool, &nodes, 600, now), vec!["node-a".to_string()]);
    }

    #[test]
    fn a_dead_owner_is_not_a_copy() {
        assert_eq!(standby_count(true, 2), 2, "targets() subtracts the owner itself");
        assert_eq!(standby_count(false, 2), 3, "one more standby replaces the dead owner");
        assert_eq!(standby_count(false, 1), 2);
    }

    #[tokio::test]
    async fn a_third_node_finds_a_dead_standbys_volume_interesting() {
        // node-c is me; rendezvous over the FULL pool would pick node-b for "v1" (pin the id so it
        // does — pick an id where targets("v1","node-a",[a,b,c],2) == [b]); over live nodes it picks c.
        let rec = Recorder::default();
        let ctx = ctx_on_node_with_routes("node-c", &rec, vec![
            Route { method: "GET", path: VOLUMES.into(), status: 200, body: list_of("Volume", vec![vol_owned_replicas("v1", "node-a", 2)]) },
            Route { method: "GET", path: WORKSPACES.into(), status: 200, body: list_of("Workspace", vec![]) },
            Route { method: "GET", path: ENVIRONMENTS.into(), status: 200, body: list_of("Environment", vec![]) },
        ]);
        let live = vec!["node-a".to_string(), "node-c".to_string()];
        assert_eq!(interesting_volumes(&ctx, &live, &live).await, vec!["v1".to_string()]);
    }
```

Node-object helpers: the existing tests build Node JSON via `node_ready`/`node_dead`; add `_obj` variants that deserialize those into `k8s_openapi::api::core::v1::Node`. If no `ctx_on_node_with_routes` exists, add the node name as a parameter to the existing constructor.

- [ ] **Step 2: Run, expect failure** — `cargo test -p kloudlite-agent -- live_nodes standby_count third_node` — FAIL.

- [ ] **Step 3: Implement**

```rust
/// Rendezvous over the FULL pool keeps electing a corpse: the reaper deletes its row every beat
/// and no live node ever becomes a target, so a volume sits one copy short until the node comes
/// back. Placement therefore sees only nodes that pass the same liveness test the reaper uses —
/// and a node with no Node object at all is dead, not unknown.
fn live_nodes(pool: &[String], nodes: &[Node], floor: i64, now: k8s_openapi::jiff::Timestamp) -> Vec<String> {
    pool.iter().filter(|n| !node_is_dead(nodes.iter().find(|k| k.name_any() == n.as_str()), floor, now)).cloned().collect()
}

/// `targets()` counts the owner as one of `total` and hands back `total - 1` standbys. A dead
/// owner holds nothing anyone can reach, so it is not a copy: ask for one standby more.
fn standby_count(owner_alive: bool, replicas: u32) -> usize {
    replicas as usize + usize::from(!owner_alive)
}
```

`pull_beat_with`: list Nodes ONCE (`Api::<Node>::all(..).list(..)`; on error warn and return — a partial view of who is alive must reap, unclaim and place nothing), pass `&nodes` into `reap_dead_replicas` and `unclaim_dead_nodes` (delete their own Node lists), then:

```rust
    let live = live_nodes(&candidates, &nodes, node_dead_secs(), now);
    for id in interesting_volumes(ctx, &live, &live).await { ... }
```

`interesting_volumes(ctx, candidates, live)`: compute `owner_alive = live.iter().any(|n| n == &v.spec.node_name)` and call `replicate::targets(&id, &v.spec.node_name, candidates, standby_count(owner_alive, v.spec.replicas))`. Keep the `i_am_owner` arm.

`pull_volume`'s "no longer a target" retirement pass must use the same live list — grep its `targets(` call (there is one more at ~line 1104's doc) and thread `live` through, or it retires the very copy the beat just made.

- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-agent && cargo clippy --workspace --all-targets --locked -- -D warnings` — PASS.

- [ ] **Step 5: Commit**

```bash
git add bins/agent/src/peer.rs
git commit -m "Place replicas over live nodes only so a dead node's copies heal elsewhere"
```

### Task 0b: Retire a copy whose rendezvous slot moved to another node

**Files:**
- Modify: `bins/agent/src/peer.rs` (`pull_beat_with`; new `retire_pass`; tests)
- Modify: `bins/agent/src/janitor.rs` (`cleanup_local` is already `pub`)

**Interfaces:**
- Consumes: `replicate::targets`, `standby_count`, `janitor::cleanup_local`, `crd::replica_name(volume, node)`, `engine.pool.voldir(id)`.
- Produces: `fn should_retire(me: &str, owner: &str, targets: &[String], hosted: bool, synced: &HashSet<String>) -> bool`; `async fn retire_pass(ctx, live: &[String])`.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn should_retire_only_an_unwanted_copy_whose_replacements_are_synced() {
        let t = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let synced = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<HashSet<_>>();
        assert!(!should_retire("b", "b", &t(&["c"]), false, &synced(&["c"])), "owner never retires");
        assert!(!should_retire("b", "a", &t(&["b"]), false, &synced(&["b"])), "still a target");
        assert!(!should_retire("b", "a", &t(&["c"]), true, &synced(&["c"])), "hosting a worktree here");
        assert!(!should_retire("b", "a", &t(&["c"]), false, &synced(&[])), "replacement not synced yet: keep");
        assert!(!should_retire("b", "", &t(&["c"]), false, &synced(&["c"])), "unowned (dead owner): keep until taken");
        assert!(should_retire("b", "a", &t(&["c"]), false, &synced(&["c"])));
    }
```

And one harness test: node-b holds `v1` locally (create `engine.pool.voldir("v1")` under the test pool), Volume `v1` owned by node-a with `replicas: 2`, live `[a, b, c]` where `targets("v1","a",live,2) == ["c"]` (pick a volume id that hashes so; assert that in the test with `replicate::targets` first), a `VolumeReplica` row for `c` in `Synced`, no Workspace hosted on b: after `retire_pass`, the recorder saw `DELETE /apis/kloudlite.io/v1alpha1/volumereplicas/{replica_name("v1","node-b")}` and `voldir("v1")` is gone. Second harness test: same but `c`'s row `Syncing` — no DELETE, directory stays.

- [ ] **Step 2: Run, expect failure** — `cargo test -p kloudlite-agent should_retire retire_pass` — FAIL.

- [ ] **Step 3: Implement**

```rust
/// A copy whose rendezvous slot moved (a node joined, or a dead one came back) is not just
/// wasted disk: its stale Synced row still wins claims and satisfies stop's flush gate with
/// data that is no longer being pulled. It goes only once every CURRENT target is Synced, so a
/// spread never passes through a moment with fewer live copies than before. An unowned volume
/// is a dead node's mid-takeover: keep everything until someone owns it again.
fn should_retire(me: &str, owner: &str, targets: &[String], hosted: bool, synced: &HashSet<String>) -> bool {
    !owner.is_empty() && owner != me && !hosted && !targets.iter().any(|t| t == me) && targets.iter().all(|t| synced.contains(t))
}

async fn retire_pass(ctx: &Arc<Ctx>, live: &[String]) {
    let vols = match Api::<crd::Volume>::all(ctx.client.clone()).list(&ListParams::default()).await {
        Ok(l) => l.items, Err(e) => { tracing::warn!(error = %e, "pull: retire: listing volumes; retiring nothing"); return; }
    };
    let rows = match Api::<crd::VolumeReplica>::all(ctx.client.clone()).list(&ListParams::default()).await {
        Ok(l) => l.items, Err(e) => { tracing::warn!(error = %e, "pull: retire: listing replicas; retiring nothing"); return; }
    };
    let hosted = hosted_volumes(ctx).await; // status.volumeRef of every Workspace/Environment with status.nodeName == me; on a list error return — retire nothing
    for v in vols {
        let id = v.name_any();
        if v.metadata.deletion_timestamp.is_some() || !ctx.engine.pool.voldir(&id).exists() { continue; }
        let owner_alive = live.iter().any(|n| n == &v.spec.node_name);
        let targets = replicate::targets(&id, &v.spec.node_name, live, standby_count(owner_alive, v.spec.replicas));
        let synced: HashSet<String> = rows.iter()
            .filter(|r| r.spec.volume == id && r.status.as_ref().is_some_and(|s| s.phase == "Synced"))
            .map(|r| r.spec.node.clone()).collect();
        if !should_retire(&ctx.node, &v.spec.node_name, &targets, hosted.contains(&id), &synced) { continue; }
        let rname = crd::replica_name(&id, &ctx.node);
        if let Err(e) = Api::<crd::VolumeReplica>::all(ctx.client.clone()).delete(&rname, &Default::default()).await {
            if !matches!(&e, kube::Error::Api(s) if s.code == 404) {
                tracing::warn!(volume = %id, error = %e, "pull: retire: deleting my replica row; keeping the copy");
                continue; // row first, copy second: a copy without a row is harmless, a row without a copy is a lie
            }
        }
        janitor::cleanup_local(&ctx.engine, &id);
        tracing::info!(volume = %id, "pull: retire: slot moved elsewhere, copy dropped");
    }
}
```

`hosted_volumes` lists Workspaces and Environments field-selected on `status.nodeName={me}` (the same selector `sync.rs` uses) and collects their `status.volumeRef.name`. Call `retire_pass(ctx, &live).await` at the end of `pull_beat_with`, after the pull loop, so a new target's pull runs before anyone retires.

- [ ] **Step 4: Run tests and clippy** — PASS.

- [ ] **Step 5: Commit**

```bash
git add bins/agent/src/peer.rs
git commit -m "Retire a replica copy once its rendezvous slot has moved and settled"
```

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
      message: "kloudlite-agent writes status, not spec (exceptions: Volume.spec.restoreTo, and Volume.spec.nodeName only owned->'' or ''->node)"
```

- [ ] **Step 3: Verify the CRD still generates and tests pass**

Run: `cargo test -p kloudlite-workspaces`
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
            Route { method: "PUT", path: "/apis/kloudlite.io/v1alpha1/workspaces/ws-run/status".into(), status: 200, body: ws_placed("ws-run", "node-b") },
            Route { method: "PUT", path: "/apis/kloudlite.io/v1alpha1/volumes/vol-ws-run/status".into(), status: 200, body: vol_owned("vol-ws-run", "node-b") },
        ]);
        unclaim_dead_nodes(&ctx).await;
        let ws = rec.body_of("PUT /apis/kloudlite.io/v1alpha1/workspaces/ws-run/status");
        assert_eq!(ws["status"]["nodeName"], "node-b", "a running worktree keeps its node");
        assert_eq!(ws["status"]["conditions"][0]["reason"], "NodeDead");
        assert!(!rec.calls().iter().any(|c| c == "PATCH /apis/kloudlite.io/v1alpha1/volumes/vol-ws-run"), "pin untouched");
        let vol = rec.body_of("PUT /apis/kloudlite.io/v1alpha1/volumes/vol-ws-run/status");
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
            Route { method: "PUT", path: "/apis/kloudlite.io/v1alpha1/workspaces/ws-stop/status".into(), status: 200, body: ws_placed_stopped("ws-stop", "") },
            Route { method: "PATCH", path: "/apis/kloudlite.io/v1alpha1/volumes/vol-ws-stop".into(), status: 200, body: vol_owned("vol-ws-stop", "") },
            Route { method: "PUT", path: "/apis/kloudlite.io/v1alpha1/volumes/vol-ws-stop/status".into(), status: 200, body: vol_owned("vol-ws-stop", "") },
        ]);
        unclaim_dead_nodes(&ctx).await;
        let ws = rec.body_of("PUT /apis/kloudlite.io/v1alpha1/workspaces/ws-stop/status");
        assert_eq!(ws["status"]["nodeName"], "");
        assert_eq!(rec.body_of("PATCH /apis/kloudlite.io/v1alpha1/volumes/vol-ws-stop")["spec"]["nodeName"], "");
        assert_eq!(rec.body_of("PUT /apis/kloudlite.io/v1alpha1/volumes/vol-ws-stop/status")["status"]["phase"], "Unavailable");
        assert!(!rec.calls().iter().any(|c| c.contains("/volumes/vol-live")));
    }
```

- [ ] **Step 2: Run, expect failure** — `cargo test -p kloudlite-agent on_a_dead_node` — FAIL.

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

- [ ] **Step 4: Run tests** — `cargo test -p kloudlite-agent` — PASS.

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
            Route { method: "PATCH", path: "/apis/kloudlite.io/v1alpha1/volumes/v1".into(), status: 200, body: vol_owned("v1", "node-a") },
        ]);
        assert!(take_volume(&ctx, "v1", "node-a").await.unwrap());
        let body = rec.body_of("PATCH /apis/kloudlite.io/v1alpha1/volumes/v1");
        assert_eq!(body[0], serde_json::json!({"op":"test","path":"/spec/nodeName","value":""}));
        assert_eq!(body[1], serde_json::json!({"op":"replace","path":"/spec/nodeName","value":"node-a"}));
    }

    #[tokio::test]
    async fn take_volume_loses_quietly_when_the_test_op_fails() {
        let rec = Recorder::default();
        let ctx = ctx_with_routes(&rec, vec![
            Route { method: "PATCH", path: "/apis/kloudlite.io/v1alpha1/volumes/v1".into(), status: 422, body: status_failure("test failed") },
        ]);
        assert!(!take_volume(&ctx, "v1", "node-a").await.unwrap());
    }
```

- [ ] **Step 2: Run, expect failure** — `cargo test -p kloudlite-agent take_volume` — FAIL (function missing).

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

- [ ] **Step 4: Run tests** — `cargo test -p kloudlite-agent` — PASS. Then `cargo clippy --workspace --all-targets --locked -- -D warnings`.

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

- [ ] **Step 2: Run, expect failure** — `cargo test -p kloudlite-agent stale_worktrees` — FAIL.

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
- Modify: `tests/ws_e2e.sh` (after the cross-node sync-point assertions; needs THREE agent nodes, else skip with a log line. Node JOIN first: with `replicas: 2` and one node's pool label removed at the start, add the label back, assert within three pull beats that the standby slot that rendezvous assigns to it (compute with the same hash in a tiny `cargo run --example` or by reading agent logs for `slot moved elsewhere`) has a `Synced` row there and that the previous standby's row and `{pool}/vol/{id}` are gone. Then node DEATH: cordon+drain is not available in the harness, so simulate by scaling the agent DaemonSet off one node with a nodeSelector label, wait `WS_NODE_DEAD_SECS`, assert within two pull beats that a `VolumeReplica` row for the volume appears on the third node and reaches `Synced`; assert a RUNNING workspace's Volume goes `Unavailable` but keeps `nodeName` and the workspace carries `NodeDead`; then stop the workspace through the API, assert the response carries the warning, the pin clears, the workspace re-claims on the other node and its Volume `spec.nodeName` equals the new `status.nodeName`)

- [ ] **Step 1: CLAUDE.md sentence**

> When a node is dead for `WS_NODE_DEAD_SECS`, the unclaim sweep marks its volumes `Unavailable` and moves ONLY the worktrees whose `desiredState` is `Stopped` — a Running one keeps its pin and a `NodeDead` condition, because its live edits exist only on the dead node and only the person may write them off by stopping it. A released volume's pin is cleared, and the node that then claims the parent takes it with a JSON-patch `test` on the empty value (`take_volume`), the one other spec write the admission policy allows.

- [ ] **Step 2: README subsection and e2e block** as described; the e2e step is skipped (not failed) with a log line when the cluster has fewer than two agent nodes.

- [ ] **Step 3: Run** `bash -n tests/ws_e2e.sh`. Commit:

```bash
git add CLAUDE.md deploy/k3s/README.md tests/ws_e2e.sh
git commit -m "Document volume takeover and assert it end to end"
```

## Self-review

- Spec coverage: replica healing (Task 0), spread on join with retire (Task 0b), sweep with the Running/Stopped split (Task 2), takeover + guard (Task 3), returning node (Task 4), `Unavailable` + admission (Task 1), stop warning + docs/caveats (Task 5). replicas:1 return-path is Task 3's takeover arm plus Task 4's empty-owner keep rule.
- The stop handler already writes `desiredState: Stopped`; nothing new fires the move — the next sweep beat does.
- Names: `take_volume`, `release_dead_volumes`, `drop_stale_worktrees`, `Phase::Unavailable` consistent across tasks.
