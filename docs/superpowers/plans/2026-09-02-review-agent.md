# Agent and Workspaces Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the WORKSPACES / AGENT findings of the 2026-09-02 codebase review: make untrusted-CR validation uniform at the agent (High #5 + the `spec.owner` Medium), get the two blocking syscalls off the reconcile reactor, scope `delete_env`'s cluster-wide list, make `VolumeReplica.spec.volume` a selectable field, replace ~9 of the ~13 per-beat cluster-wide LISTs with one shared listing, split the 3162-line `controller.rs` along its seven existing seams, delete the dead code, and add the seven missing tests.

**Architecture:** Three tiers, unchanged. `crates/workspaces` holds the CRDs, the pod/policy builders and `/v1`; `bins/agent` is the node-scoped controller (one DaemonSet pod per btrfs node) plus three background beats (pull, sync, janitor); `bins/api` serves `/v1`. This plan adds exactly one new module, `bins/agent/src/listing.rs` (the shared per-beat listing), and one new directory, `bins/agent/src/controller/` (a pure file split — no logic moves). Everything else is an edit in place.

**Tech Stack:** Rust 2021, `kube`/`kube-runtime` + `k8s-openapi`, `axum` for the peer listener and `/v1`, `tokio` (`spawn_blocking` for every blocking syscall), `serde_json` for the CRD JSON shapes, `kloudlite_git_workspaces::kube_test::{mock_client, Route, Recorder}` for every API-server test.

**Spec:** docs/superpowers/reviews/2026-09-02-codebase-review.md (details: docs/superpowers/reviews/2026-09-02-details/workspaces-agent.md)

## Global Constraints
- **Keep-bias:** every list error in a delete/retire/unclaim/retention path aborts the pass with nothing deleted — never "empty list means nothing to keep". This applies to every listing this plan moves or shares.
- **Two-level validation:** `/v1` validates on write AND the agent re-validates before the value becomes an argv, a path, an object name or a mount target — a CR written by kubectl, a restore or a migration must fail closed at the agent.
- **Never authorize on a label:** label selectors narrow a query; `spec.owner` / `status.nodeName` decides.
- Comments explain WHY, never what; match the density of `bins/agent/src/controller.rs`.
- Deliberate shortcuts carry `// ponytail: <ceiling and upgrade path>`; keep every existing marker when editing near one.
- Commit subjects: imperative sentence case, no tool attribution, no trailers.
- Every controller-split task is a PURE MOVE: `cargo test` output unchanged, `git diff --stat` shows only deletions in `controller.rs` and additions in the new file (plus `mod`/`use` lines).
- `cargo clippy --workspace --all-targets --locked -- -D warnings` must pass; the bar in test targets is no NEW warnings in files you touch.
- Package names: `-p kloudlite-git-agent-bin` (bins/agent), `-p kloudlite-git-workspaces` (crates/workspaces).

---

### Task 1: One `validate_spec` at the agent — workspace name and owner

Closes detail findings 1 and 2, summary High #5, the `spec.owner` Medium and Architecture #3. `spec.name` reaches a root `/bin/sh -c` prelude (`k8s.rs:384-407`), the sshd `SetEnv` list and the container `mount_path` (`k8s.rs:905`) with no re-check; `spec.owner` reaches `homes_root(pool).join(owner)` + a root `chown` (`controller.rs:1917-1924`) and `{pool}/homecache/{owner}` with none either — blocked today only because `heal_labels` patches the same string as a label first.

**Files:**
- Modify `crates/workspaces/src/k8s.rs:847` — `workspace_pod` signature becomes fallible.
- Modify `bins/agent/src/controller.rs:1928-1930` — `apply_workspace` opens with `validate_spec`; `:2287` — the `workspace_pod` call site.
- Modify `bins/agent/src/controller.rs:2364-2365` — `apply_environment` opens with the owner guard.
- Test `crates/workspaces/src/k8s.rs` (in-crate `mod tests`, currently starting at `:1378`).
- Test `bins/agent/tests/reconcile.rs` (append).

**Interfaces:**
- Produces `pub fn workspace_pod(spec: &WorkspaceSpec, id: &str, ws_id: &str, ctx: &PodContext, init: Option<Container>) -> Result<Pod, String>` (was `-> Pod`).
- Produces `pub fn validate_ws_spec(spec: &crd::WorkspaceSpec) -> Result<(), String>` in `crates/workspaces/src/model.rs`.
- Consumes existing `model::valid_ws_name` (`model.rs:182`) and `kloudlite_git_storage::store::valid_owner`.
- Consumed by Task 21 (the hostile-name test) and by the controller split (Task 14 moves the call site verbatim).

- [ ] **Step 1: Write the failing test** — append to `crates/workspaces/src/k8s.rs`'s `mod tests`:

```rust
    /// `spec.name` is spliced into a root `/bin/sh -c` prelude, the sshd `SetEnv` list and the
    /// container's `mount_path`. `/v1` checks it, but a Workspace written by any other path — a
    /// restored backup, a migration, an operator with kubectl — reaches this builder directly,
    /// which is exactly why `git_init_container` and `service_statefulset` both re-check.
    #[test]
    fn workspace_pod_refuses_a_name_that_is_not_a_name() {
        let ctx = PodContext {
            pool: "/wspool",
            node_name: "node-a",
            owner_ref: owner_ref(),
            runtime_class: None,
            default_image: "img:1",
        };
        for hostile in ["../../etc", "a; touch /pwned", "", "..", "x'\nchown 0 /", &"n".repeat(64)] {
            let spec: crd::WorkspaceSpec = serde_json::from_value(serde_json::json!({
                "owner": "alice", "team": "", "name": hostile, "region": "r1",
                "image": "", "packages": [], "desiredState": "running",
            }))
            .unwrap();
            assert!(workspace_pod(&spec, "vol-1", "ws-1", &ctx, None).is_err(), "accepted {hostile:?}");
        }
    }

    /// The ordinary name still builds, and still mounts where it always did.
    #[test]
    fn workspace_pod_accepts_a_real_name() {
        let ctx = PodContext {
            pool: "/wspool",
            node_name: "node-a",
            owner_ref: owner_ref(),
            runtime_class: None,
            default_image: "img:1",
        };
        let spec: crd::WorkspaceSpec = serde_json::from_value(serde_json::json!({
            "owner": "alice", "team": "", "name": "my-ws", "region": "r1",
            "image": "", "packages": [], "desiredState": "running",
        }))
        .unwrap();
        let pod = workspace_pod(&spec, "vol-1", "ws-1", &ctx, None).expect("a real name builds");
        let mounts = pod.spec.unwrap().containers[0].volume_mounts.clone().unwrap();
        assert!(mounts.iter().any(|m| m.mount_path == workspace_dir("my-ws")));
    }
```

  and append to `bins/agent/tests/reconcile.rs`:

```rust
/// The owner guard: `spec.owner` becomes `{pool}/homes/{owner}` and is chowned by a privileged
/// process, so a traversing owner must settle Permanent before `ensure_shared_home` runs — not be
/// caught by accident because `heal_labels` patches the same string as a label first.
#[tokio::test]
async fn a_workspace_with_a_traversing_owner_settles_permanent_and_makes_no_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![patch_ok("/apis/kloudlite-git.io/v1alpha1/workspaces/ws-1/status")]);
    let w: crd::Workspace = serde_json::from_value(serde_json::json!({
        "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": "ws-1", "uid": "ws-uid", "generation": 1},
        "spec": {"owner": "../../etc", "team": "", "name": "ws", "region": "r1",
                 "image": "", "packages": [], "desiredState": "running"},
    }))
    .unwrap();

    let action = kloudlite_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "permanent, never retried");
    let sent = rec.sent("PATCH", "/apis/kloudlite-git.io/v1alpha1/workspaces/ws-1/status");
    let reason = sent.last().expect("a status write")["status"]["conditions"][0]["reason"].clone();
    assert_eq!(reason, "InvalidSpec");
    assert!(!tmp.path().join("homes").exists(), "nothing under the pool root was created");
}
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-git-workspaces workspace_pod_refuses` fails to compile with `error[E0599]: no method named 'is_err' found for struct 'Pod'`; `cargo test -p kloudlite-git-agent-bin a_workspace_with_a_traversing_owner` fails on `assertion 'left == right' failed: left: String("HomeNotReady")` (or a mock 404), never `InvalidSpec`.

- [ ] **Step 3: Implement** — in `crates/workspaces/src/model.rs`, after `valid_ws_name` (`:190`):

```rust
/// Every untrusted string on a `WorkspaceSpec` that becomes a path, an argv word or an object
/// name, checked in ONE place at the agent.
///
/// `/v1` checks these on write (`api.rs`'s `check_ws_name`), but `/v1` is not the only writer: a
/// restored backup, a migration or an operator with kubectl produces a spec no handler ever saw,
/// and the agent's own builders splice these into a root `/bin/sh -c` prelude and into
/// `{pool}/homes/{owner}`. Same rule, same reason, as `git_init_container`'s repo/branch re-check.
pub fn validate_ws_spec(spec: &crate::crd::WorkspaceSpec) -> Result<(), String> {
    if !valid_ws_name(&spec.name) {
        return Err(format!("workspace name {:?} is not a name", spec.name));
    }
    validate_owner(&spec.owner)?;
    if !spec.team.is_empty() && !kloudlite_git_storage::store::valid_segment(&spec.team) {
        return Err(format!("team {:?} is not a segment", spec.team));
    }
    crate::packages::validate_list(&spec.packages)
}

/// `spec.owner` alone — the half an `Environment` shares. It is joined onto the pool root and
/// chowned by a privileged process (`ensure_shared_home`, `ensure_homecache`), so a traversal here
/// is a root-run `mkdir`/`chown` outside the pool.
pub fn validate_owner(owner: &str) -> Result<(), String> {
    match kloudlite_git_storage::store::valid_owner(owner) {
        true => Ok(()),
        false => Err(format!("owner {owner:?} is not an owner name")),
    }
}
```

  In `crates/workspaces/src/k8s.rs:847`, change the signature and open the body:

```rust
pub fn workspace_pod(
    spec: &WorkspaceSpec,
    id: &str,
    ws_id: &str,
    ctx: &PodContext,
    init: Option<Container>,
) -> Result<Pod, String> {
    // The last place before `spec.name` becomes a root `/bin/sh -c` word, an sshd `SetEnv` value
    // and this container's `mount_path`. `/v1` checked it; this covers a Workspace written by any
    // other path, exactly as `git_init_container` and `service_statefulset` do for their inputs.
    if !crate::model::valid_ws_name(&spec.name) {
        return Err(format!("workspace name {:?} is not a name", spec.name));
    }
```

  and end the function with `Ok(pod)` instead of the bare `pod` (the tail expression at the end of `workspace_pod`).

  In `bins/agent/src/controller.rs`, replace line `1929` (the `heal_labels` call that opens `apply_workspace`) so validation runs FIRST:

```rust
pub async fn apply_workspace(w: &crd::Workspace, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    let gen = w.meta().generation.unwrap_or(0);
    // BEFORE `heal_labels`, and before anything reads the spec: the label patch happens to reject
    // a `/` today, which is the only thing standing between `spec.owner` and a root-run
    // `mkdir`/`chown` under the pool root. Do not rely on a cosmetic call failing first.
    if let Err(why) = model::validate_ws_spec(&w.spec) {
        let prev = w.status.clone().unwrap_or_default();
        return settle(
            Outcome::Permanent(why, "InvalidSpec"),
            w,
            "Workspace",
            gen,
            move |cond| {
                serde_json::json!({
                    "phase": crd::Phase::Error,
                    "conditions": kept_conditions(&prev.conditions, cond),
                })
            },
            ctx,
        )
        .await;
    }
    heal_labels(&Api::<crd::Workspace>::all(ctx.client.clone()), w, &w.spec.owner, &w.spec.team, "workspace").await?;
    let mut prev = w.status.clone().unwrap_or_default();
```

  (delete the now-duplicated `let gen = …` at the old `:1930` and the old `let mut prev = …`.)

  In `apply_environment` (`controller.rs:2364`), the same shape for the owner half only:

```rust
pub async fn apply_environment(e: &crd::Environment, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    let gen = e.meta().generation.unwrap_or(0);
    // `spec.owner` reaches `ensure_homecache`'s `{pool}/homecache/{owner}` here too.
    if let Err(why) = model::validate_owner(&e.spec.owner) {
        let prev = e.status.clone().unwrap_or_default();
        return settle(
            Outcome::Permanent(why, "InvalidSpec"),
            e,
            "Environment",
            gen,
            move |cond| serde_json::json!({"phase": crd::Phase::Error, "conditions": vec![cond]}),
            ctx,
        )
        .await;
    }
    heal_labels(&Api::<crd::Environment>::all(ctx.client.clone()), e, &e.spec.owner, "", "environment").await?;
    let prev = e.status.clone().unwrap_or_default();
```

  (delete the old `let gen = …` line.)

  At the `workspace_pod` call site (`controller.rs:2287`), settle the `Err` the way `git_init_container`'s already is at `:2248-2267`:

```rust
            let pod = match k8s::workspace_pod(&w.spec, &id, &w.name_any(), &pod_ctx, init) {
                Ok(p) => p,
                // Unreachable while `validate_ws_spec` runs at the top of this function; kept
                // because the builder is the boundary and must be able to say no on its own.
                Err(why) => {
                    let prev = prev.clone();
                    return settle(
                        Outcome::Permanent(why, "InvalidName"),
                        w,
                        "Workspace",
                        gen,
                        move |cond| {
                            serde_json::json!({
                                "phase": crd::Phase::Error,
                                "nodeName": prev.node_name,
                                "compatibleNodes": prev.compatible_nodes,
                                "volumeRef": prev.volume_ref,
                                "conditions": kept_conditions(&prev.conditions, cond),
                            })
                        },
                        ctx,
                    )
                    .await;
                }
            };
            create_if_absent(&pods, &pod).await?;
```

- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-git-workspaces && cargo test -p kloudlite-git-agent-bin && cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] **Step 5: Commit** — `git add crates/workspaces/src/model.rs crates/workspaces/src/k8s.rs bins/agent/src/controller.rs bins/agent/tests/reconcile.rs && git commit -m "Validate a workspace spec at the agent before it becomes a path or an argv"`

---

### Task 2: Get the two blocking syscalls off the reconcile reactor

Detail finding 4, summary Medium. `ensure_shared_home` (`controller.rs:2006`) runs `timeout -s KILL 5 ls`, `umount -f -l` and `timeout -s KILL 60 nsenter … mount` synchronously — up to ~65 s of reactor block — while the very next line (`:2008`) already wraps `ensure_homecache` in `spawn_blocking`. `write_resolv_conf` (`:1685`, called at `:2157`) does sync `create_dir_all`/`read_to_string`/`write` on every workspace pass.

**Files:** Modify `bins/agent/src/controller.rs:2006` and `:2157`.

**Interfaces:** Consumes `ensure_shared_home(pool: &str, export: &str, owner: &str, uid: u32) -> Result<(), String>` (`:1910`) and `write_resolv_conf(pool: &str, ws_id: &str, ws_ns: &str, env_ns: Option<&str>) -> Result<(), ReconcileErr>` (`:1685`) — both unchanged; only the call sites move onto a blocking thread.

- [ ] **Step 1: Write the failing test** — append to `bins/agent/tests/reconcile.rs`:

```rust
/// The reactor must stay free while the shared-home mount check runs: `mount_homes` shells out to
/// `ls`, `umount -f -l` and `nsenter … mount`, up to ~65 s of synchronous syscalls. Asserted by
/// running a reconcile on a single-threaded runtime beside a timer — a blocking call on the
/// reactor starves the timer, a `spawn_blocking` one does not.
#[tokio::test(flavor = "current_thread")]
async fn a_workspace_reconcile_never_blocks_the_reactor() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, _rec) = ctx(tmp.path(), vec![patch_ok("/apis/kloudlite-git.io/v1alpha1/workspaces/ws-1/status")]);
    let w = workspace_json_running("ws-1");
    let w: crd::Workspace = serde_json::from_value(w).unwrap();

    let ticked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = ticked.clone();
    let timer = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let _ = kloudlite_git_agent::controller::apply_workspace(&w, &ctx).await;
    timer.await.unwrap();
    assert!(ticked.load(std::sync::atomic::Ordering::SeqCst), "the reactor made no progress during the reconcile");
}
```

  (Add the `workspace_json_running` fixture beside the existing workspace fixtures in the file if one does not already exist, mirroring `volume_json`.)

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-git-agent-bin a_workspace_reconcile_never_blocks_the_reactor` — before the fix the test is a regression guard that passes only by luck on a fast fs; treat the real gate as the `git diff` review plus clippy. Expected failure on a machine where `/etc/resolv.conf` is slow: `the reactor made no progress during the reconcile`.

- [ ] **Step 3: Implement** — replace `controller.rs:2006`:

```rust
    // `spawn_blocking`, exactly as the `ensure_homecache` call below: `mount_homes` runs
    // `timeout -s KILL 5 ls`, `umount -f -l` and `timeout -s KILL 60 nsenter … mount`, all
    // synchronous — up to ~65 s of a reactor thread that every other workspace on this node shares.
    let (pool, export_owned, owner) = (ctx.pool.clone(), export.to_string(), w.spec.owner.clone());
    tokio::task::spawn_blocking(move || ensure_shared_home(&pool, &export_owned, &owner, k8s::SSH_UID as u32))
        .await
        .map_err(|e| ReconcileErr(e.to_string()))?
        .map_err(ReconcileErr)?;
```

  and `controller.rs:2157`:

```rust
    // Same rule: `create_dir_all` + `read_to_string` + `write`, on the shared home's NFS mount in
    // the worst case, on every workspace pass.
    let (pool, ws_id, ns_owned, env_ns_owned) =
        (ctx.pool.clone(), w.name_any(), ns.clone(), env_ns.clone());
    tokio::task::spawn_blocking(move || write_resolv_conf(&pool, &ws_id, &ns_owned, env_ns_owned.as_deref()))
        .await
        .map_err(|e| ReconcileErr(e.to_string()))??;
```

- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-git-agent-bin && cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] **Step 5: Commit** — `git add bins/agent/src/controller.rs bins/agent/tests/reconcile.rs && git commit -m "Run the shared-home mount check and the resolv.conf write off the reactor"`

---

### Task 3: Scope `delete_env`'s workspace list and stop swallowing its error

Detail finding 3, summary Medium. `crates/workspaces/src/api.rs:1465` is `if let Ok(list) = wss.list(&ListParams::default()).await` — unfiltered, cluster-wide, on every environment delete, with a silent `Err` arm. Every other listing in the file goes through `owned_by`/`owned_in` (`api.rs:314-322`).

**Files:** Modify `crates/workspaces/src/api.rs:1462-1472`. Test `crates/workspaces/tests/api_user.rs` (append).

**Interfaces:** Consumes `owned_by(owner: &str) -> ListParams` (`api.rs:313`). No new signatures.

- [ ] **Step 1: Write the failing test** — append to `crates/workspaces/tests/api_user.rs`, in the shape of the existing `/v1` handler tests in that file:

```rust
/// Deleting an environment clears its attachments — with an OWNER-scoped list, never a
/// cluster-wide one: `attach_ws` refuses cross-region and requires the caller to own both, so
/// every workspace that could legitimately be attached carries this owner's label.
#[tokio::test]
async fn delete_env_lists_only_the_owner_s_workspaces() {
    let (app, rec) = api_with_kube(vec![
        kloudlite_git_workspaces::kube_test::get(
            "/apis/kloudlite-git.io/v1alpha1/environments/env-1",
            env_json("env-1", "alice"),
        ),
        Route { method: "DELETE", path: "/apis/kloudlite-git.io/v1alpha1/environments/env-1".into(), status: 200, body: env_json("env-1", "alice") },
        kloudlite_git_workspaces::kube_test::get(
            "/apis/kloudlite-git.io/v1alpha1/workspaces",
            serde_json::json!({"apiVersion": "v1", "kind": "WorkspaceList", "items": []}),
        ),
    ])
    .await;

    let resp = call(&app, "DELETE", "/v1/environments/env-1", None).await;
    assert_eq!(resp.status(), 202);
    let listed = rec
        .requests()
        .into_iter()
        .find(|r| r.starts_with("GET /apis/kloudlite-git.io/v1alpha1/workspaces?"))
        .expect("the attachment sweep listed workspaces");
    assert!(listed.contains("kloudlite-git.io%2Fowner%3Dalice") || listed.contains("kloudlite-git.io/owner=alice"), "{listed}");
}
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-git-workspaces --test api_user delete_env_lists_only` fails with `the attachment sweep listed workspaces` unmatched on the selector assertion (the request carries no `labelSelector` at all).

- [ ] **Step 3: Implement** — replace `crates/workspaces/src/api.rs:1464-1473`:

```rust
    let wss: Api<crd::Workspace> = Api::all(c.clone());
    // Owner-scoped, like every other listing in this file (`owned_by`'s comment says why): an
    // unfiltered cluster-wide list on every environment delete is a full Workspace scan, and
    // `attach_ws` refuses a cross-owner or cross-region attachment, so this selector cannot miss
    // one. The `Err` arm is LOGGED, not dropped: a failed list leaves workspaces pointing at a
    // deleted environment, and the reconciler treating that as unattached is a degradation
    // somebody has to be able to find in the logs.
    match wss.list(&owned_by(&e.spec.owner)).await {
        Ok(list) => {
            for w in list.items.iter().filter(|w| w.spec.attached_environment.as_deref() == Some(id.as_str())) {
                let patch = serde_json::json!({"spec": {"attachedEnvironment": serde_json::Value::Null}});
                if let Err(e) = wss.patch(&w.name_any(), &PatchParams::default(), &Patch::Merge(&patch)).await {
                    tracing::warn!(workspace = %w.name_any(), error = %e, "clearing an attachment");
                }
            }
        }
        Err(err) => tracing::warn!(environment = %id, error = %err, "listing workspaces to clear attachments; some may still name this environment"),
    }
```

- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-git-workspaces && cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] **Step 5: Commit** — `git add crates/workspaces/src/api.rs crates/workspaces/tests/api_user.rs && git commit -m "Scope the environment delete's attachment sweep to the owner and log its errors"`

---

### Task 4: Make `VolumeReplica.spec.volume` a selectable field

Detail finding 9, summary Medium. `flush_gate` (`controller.rs:3002`) lists EVERY `VolumeReplica` on the 15 s `TICK` for every stopping parent and filters client-side; `pull_volume` (`peer.rs:501`) does the same per volume per beat. Both comments correctly say a `spec.volume` field selector is a 400 today — because the CRD declares only `.spec.node` and `.status.phase` (`crd.rs:281-282`).

**⚠ Rollout order:** `deploy/k3s/crds.yaml` must be applied to the cluster BEFORE the agent image that uses the new selector rolls. An agent asking for an unsupported field selector gets a 400 on every reconcile, which parks stops forever (the comment at `controller.rs:2994-2999` records exactly that outage). Note this in the commit body and in `deploy/k3s/README.md`.

**Files:**
- Modify `crates/workspaces/src/crd.rs:279-283` (the `#[kube(...)]` attribute on `VolumeReplicaSpec`).
- Regenerate `deploy/k3s/crds.yaml`.
- Modify `crates/workspaces/tests/crd_yaml.rs:39` (the `"VolumeReplica"` arm of `every_crd_has_a_status_subresource_and_the_right_node_selector`).
- Modify `bins/agent/src/controller.rs:2994-3003` and `bins/agent/src/peer.rs:497-504`.
- Modify `deploy/k3s/README.md` (apply-order note).

**Interfaces:** Produces `.spec.volume` as a `selectableField` on `VolumeReplica`. Consumed by `flush_gate` and `pull_volume` in this task, and by Task 6's shared listing (which keeps the cluster-wide replica list for the beat but no longer needs a per-volume one).

- [ ] **Step 1: Write the failing test** — in `crates/workspaces/tests/crd_yaml.rs`, change the `VolumeReplica` arm:

```rust
            // `.spec.volume`: `flush_gate` and `pull_volume` both filtered client-side because a
            // selector on it was a 400. Dropping it makes both a full-cluster replica scan again.
            "VolumeReplica" => &[".spec.node", ".status.phase", ".spec.volume"],
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-git-workspaces --test crd_yaml` fails with `VolumeReplica must select on .spec.volume`.

- [ ] **Step 3: Implement** — in `crates/workspaces/src/crd.rs`, add one line to the `VolumeReplica` `#[kube(...)]` attribute after `selectable = ".status.phase",` (`:282`):

```rust
    // Scopes `flush_gate`'s per-tick replica list and `pull_volume`'s per-volume one. Apply
    // `deploy/k3s/crds.yaml` BEFORE rolling an agent that uses it: an unsupported field selector
    // is a 400 on every reconcile, which parked real stops for good the last time it happened.
    selectable = ".spec.volume",
```

  Regenerate: `CRD_REGEN=1 cargo test -p kloudlite-git-workspaces --test crd_yaml`.

  Replace `bins/agent/src/controller.rs:2994-3003` (the comment block and the list):

```rust
    // Server-side now that `VolumeReplica` declares `.spec.volume` selectable — this ran once per
    // 15 s tick per stopping parent as a full-cluster replica scan. The `spec.volume` re-check
    // below stays: a field selector narrows a query and is never what decides anything.
    let lp = kube::api::ListParams::default().fields(&format!("spec.volume={volume}"));
    let list = Api::<crd::VolumeReplica>::all(ctx.client.clone()).list(&lp).await?;
```

  Replace `bins/agent/src/peer.rs:497-504`:

```rust
    let lp = ListParams::default().fields(&format!("spec.volume={volume}"));
    let replicas: Vec<crd::VolumeReplica> = match Api::<crd::VolumeReplica>::all(ctx.client.clone()).list(&lp).await {
        Ok(list) => list.items.into_iter().filter(|r| r.spec.volume == volume).collect(),
        Err(e) => {
            tracing::warn!(%volume, error = %e, "pull: listing replicas; nothing to pull from");
            Vec::new()
        }
    };
```

  Add to `deploy/k3s/README.md`, in the apply section: `crds.yaml` must be applied before the agent image that reads `.spec.volume` on `VolumeReplica` (added 2026-09-02); an agent ahead of the CRD gets a 400 on every stop's flush gate.

- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-git-workspaces && cargo test -p kloudlite-git-agent-bin && cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] **Step 5: Commit** — `git add crates/workspaces/src/crd.rs crates/workspaces/tests/crd_yaml.rs deploy/k3s/crds.yaml deploy/k3s/README.md bins/agent/src/controller.rs bins/agent/src/peer.rs && git commit -m "Select volume replicas by spec.volume instead of scanning the cluster"`

---

### Task 5: The shared per-beat listing

Architecture note #1 and the plumbing half of detail finding 7 / summary High #6. Defines the struct and the one listing function; no consumer moves yet.

**Files:** Create `bins/agent/src/listing.rs`. Modify `bins/agent/src/lib.rs` (add `pub mod listing;` beside the existing `pub mod` lines).

**Interfaces — this is the contract every later task consumes:**

```rust
pub struct Parent {
    pub kind: &'static str,          // "Workspace" | "Environment"
    pub name: String,                // the CR name — the WORKTREE name
    pub volume: String,              // status.volumeRef
    pub owner: String,               // spec.owner
    pub head: Option<String>,        // status.head
    pub phase: crd::Phase,           // status.phase
    pub pod_ref: Option<String>,     // status.podRef ("" for an Environment)
    pub owner_ref: OwnerReference,
}
pub struct Beat {
    pub volumes: Vec<crd::Volume>,
    pub replicas: Vec<crd::VolumeReplica>,
    pub parents: Vec<Parent>,        // ON THIS NODE ONLY (status.nodeName == ctx.node)
}
pub async fn parents_on_node(ctx: &Arc<Ctx>) -> Option<Vec<Parent>>;
pub async fn beat(ctx: &Arc<Ctx>) -> Option<Beat>;
impl Beat { pub fn hosted_volumes(&self) -> HashSet<String>; }
impl Parent { pub fn is_live_worktree(&self) -> bool; }
```

`None` from either function means "the cluster could not be fully listed" — keep-bias: every caller must do nothing rather than act on a partial view.

- [ ] **Step 1: Write the failing test** — create `bins/agent/src/listing.rs` with only its `#[cfg(test)] mod tests`, copying `peer.rs`'s `reconcile_tests` harness shape (`peer.rs:1058-1138`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use kloudlite_git_workspaces::engine::{Engine, Pool as EnginePool};
    use kloudlite_git_workspaces::kube_test::{mock_client, Recorder, Route};

    struct NoopNix;
    #[async_trait::async_trait]
    impl crate::nix::Nix for NoopNix {
        async fn build(&self, _e: &str, _t: std::time::Duration) -> Result<std::path::PathBuf, String> {
            Ok(std::path::PathBuf::from("/tmp"))
        }
        async fn ping(&self) -> Result<(), String> { Ok(()) }
        async fn collect_garbage(&self) -> Result<u64, String> { Ok(0) }
    }

    fn test_ctx(pool: &std::path::Path, node: &str, routes: Vec<Route>) -> (Arc<Ctx>, Recorder) {
        let (client, rec) = mock_client(routes);
        std::env::set_var("WS_DEFAULT_IMAGE", "ghcr.io/kloudlite/kloudlite-git-workspace:deadbeef");
        (
            Arc::new(Ctx::new(
                client,
                Arc::new(Engine::new(EnginePool::new(pool))),
                node.into(),
                pool.to_string_lossy().into(),
                "r1".into(),
                vec![],
                Some("test:/".into()),
                Arc::new(NoopNix),
                pool.join("profiles"),
            )),
            rec,
        )
    }

    fn list_of(kind: &str, items: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({"apiVersion": "v1", "kind": format!("{kind}List"), "items": items})
    }

    const VOLUMES: &str = "/apis/kloudlite-git.io/v1alpha1/volumes";
    const VOLREPLICAS: &str = "/apis/kloudlite-git.io/v1alpha1/volumereplicas";
    const WORKSPACES: &str = "/apis/kloudlite-git.io/v1alpha1/workspaces";
    const ENVIRONMENTS: &str = "/apis/kloudlite-git.io/v1alpha1/environments";

    fn ws(name: &str, node: &str, volume: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "Workspace",
            "metadata": {"name": name, "uid": format!("{name}-uid")},
            "spec": {"owner": "alice", "team": "", "name": name, "region": "r1",
                     "image": "", "packages": [], "desiredState": "running"},
            "status": {"phase": "ready", "nodeName": node, "volumeRef": volume,
                       "podRef": format!("ws-alice/{name}"), "head": "v1-aaaa"},
        })
    }

    /// FOUR listings, not thirteen: one Volume, one VolumeReplica, one Workspace, one Environment,
    /// with the two parent kinds scoped server-side to this node.
    #[tokio::test]
    async fn one_beat_is_four_listings_and_the_parents_are_node_scoped() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![
            Route { method: "GET", path: VOLUMES.into(), status: 200, body: list_of("Volume", vec![]) },
            Route { method: "GET", path: VOLREPLICAS.into(), status: 200, body: list_of("VolumeReplica", vec![]) },
            Route { method: "GET", path: WORKSPACES.into(), status: 200, body: list_of("Workspace", vec![ws("ws-1", "node-a", "v1")]) },
            Route { method: "GET", path: ENVIRONMENTS.into(), status: 200, body: list_of("Environment", vec![]) },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        let b = beat(&ctx).await.expect("a full listing");
        assert_eq!(b.parents.len(), 1);
        assert_eq!(b.parents[0].volume, "v1");
        assert_eq!(b.hosted_volumes(), ["v1".to_string()].into_iter().collect());
        assert_eq!(rec.calls().iter().filter(|c| c.starts_with("GET /apis")).count(), 4, "{:?}", rec.calls());
        let ws_req = rec.requests().into_iter().find(|r| r.contains("/workspaces?")).expect("a workspace list");
        assert!(ws_req.contains("status.nodeName%3Dnode-a") || ws_req.contains("status.nodeName=node-a"), "{ws_req}");
    }

    /// Keep-bias: a listing that could not be completed is `None`, never a short list — every
    /// consumer of this decides what to delete, retire or unclaim from it.
    #[tokio::test]
    async fn a_failed_listing_is_none_not_an_empty_beat() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![
            Route { method: "GET", path: VOLUMES.into(), status: 500, body: serde_json::json!({}) },
        ];
        let (ctx, _rec) = test_ctx(tmp.path(), "node-a", routes);
        assert!(beat(&ctx).await.is_none());
    }

    /// A parent whose status names another node is not on this node, whatever the selector did:
    /// the field selector narrows the query, the check decides.
    #[tokio::test]
    async fn a_parent_claimed_elsewhere_is_not_mine() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![
            Route { method: "GET", path: WORKSPACES.into(), status: 200, body: list_of("Workspace", vec![ws("ws-1", "node-b", "v1")]) },
            Route { method: "GET", path: ENVIRONMENTS.into(), status: 200, body: list_of("Environment", vec![]) },
        ];
        let (ctx, _rec) = test_ctx(tmp.path(), "node-a", routes);
        assert!(parents_on_node(&ctx).await.expect("listed").is_empty());
    }
}
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-git-agent-bin --lib listing::` fails to compile: `error[E0425]: cannot find function 'beat' in this scope`.

- [ ] **Step 3: Implement** — the body of `bins/agent/src/listing.rs`, above the test module:

```rust
//! The ONE listing every per-node beat shares.
//!
//! `pull_beat_with` used to make ~13 cluster-wide LISTs per node per beat plus a full
//! `VolumeReplica` list per volume inside `pull_volume`, and `sync::live_worktrees` and
//! `snapshot::worktree_heads` each made their own copy of the same "parents on this node, by
//! volumeRef" query. This module is that query, written once and threaded through — the same shape
//! `nodes`/`floor`/`now` already take in `pull_beat_with`, and for the same reason: every decision
//! in one beat must agree on one view of the cluster.
//!
//! `None` means the cluster could not be fully listed. It is NOT an empty result: every consumer
//! here decides what to delete, retire or unclaim, and a partial view is exactly the case that
//! would drop a copy nobody else holds.

use crate::controller::Ctx;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::ListParams;
use kube::{Api, ResourceExt};
use kloudlite_git_workspaces::crd;
use std::collections::HashSet;
use std::sync::Arc;

/// A `Workspace` or an `Environment` claimed by this node, flattened to the fields the beats
/// actually read. The two kinds share no status type, which is the whole reason four copies of
/// this query existed.
#[derive(Clone, Debug)]
pub struct Parent {
    pub kind: &'static str,
    /// The CR name, which is also the WORKTREE name (`Pool::worktree`) — never the volume's.
    pub name: String,
    pub volume: String,
    pub owner: String,
    pub head: Option<String>,
    pub phase: crd::Phase,
    /// `None` for an Environment: it has no single pod, and `is_live_worktree` says so.
    pub pod_ref: Option<String>,
    pub owner_ref: OwnerReference,
}

impl Parent {
    /// Something is writing to this worktree right now, so the sync beat has a generation to read.
    /// A workspace needs a pod (without one nothing writes and its last sync point is current);
    /// an environment's StatefulSets are not a single `podRef`, so `Stopped` is the only bar.
    pub fn is_live_worktree(&self) -> bool {
        self.phase != crd::Phase::Stopped && (self.kind == "Environment" || self.pod_ref.is_some())
    }
}

/// Volumes and replicas cluster-wide (placement is a cluster-wide decision), parents scoped to
/// this node.
pub struct Beat {
    pub volumes: Vec<crd::Volume>,
    pub replicas: Vec<crd::VolumeReplica>,
    pub parents: Vec<Parent>,
}

impl Beat {
    /// The volumes a worktree runs against on this node — never retire or release one of these.
    pub fn hosted_volumes(&self) -> HashSet<String> {
        self.parents.iter().map(|p| p.volume.clone()).collect()
    }
}

/// Both parent kinds, this node's only. Server-side scoping via `status.nodeName`, which both CRDs
/// declare selectable; the local re-check stays because a cluster on an older CRD would hand back
/// every node's objects and this node would act on someone else's.
pub async fn parents_on_node(ctx: &Arc<Ctx>) -> Option<Vec<Parent>> {
    let mine = ListParams::default().fields(&format!("status.nodeName={}", ctx.node));
    let mut out = Vec::new();
    match Api::<crd::Workspace>::all(ctx.client.clone()).list(&mine).await {
        Ok(list) => {
            for w in &list.items {
                let Some(st) = w.status.as_ref() else { continue };
                let (Some(volume), Ok(owner_ref)) =
                    (st.volume_ref.clone(), crate::controller::owner_ref_of_kind(w))
                else {
                    continue;
                };
                if st.node_name != ctx.node {
                    continue;
                }
                out.push(Parent {
                    kind: "Workspace",
                    name: w.name_any(),
                    volume,
                    owner: w.spec.owner.clone(),
                    head: st.head.clone(),
                    phase: st.phase,
                    pod_ref: st.pod_ref.clone(),
                    owner_ref,
                });
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "listing this node's workspaces; this beat does nothing");
            return None;
        }
    }
    match Api::<crd::Environment>::all(ctx.client.clone()).list(&mine).await {
        Ok(list) => {
            for e in &list.items {
                let Some(st) = e.status.as_ref() else { continue };
                let (Some(volume), Ok(owner_ref)) =
                    (st.volume_ref.clone(), crate::controller::owner_ref_of_kind(e))
                else {
                    continue;
                };
                if st.node_name != ctx.node {
                    continue;
                }
                out.push(Parent {
                    kind: "Environment",
                    name: e.name_any(),
                    volume,
                    owner: e.spec.owner.clone(),
                    head: st.head.clone(),
                    phase: st.phase,
                    pod_ref: None,
                    owner_ref,
                });
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "listing this node's environments; this beat does nothing");
            return None;
        }
    }
    Some(out)
}

/// Everything one pull beat reads about the cluster: four listings, once.
pub async fn beat(ctx: &Arc<Ctx>) -> Option<Beat> {
    let volumes = match Api::<crd::Volume>::all(ctx.client.clone()).list(&ListParams::default()).await {
        Ok(l) => l.items,
        Err(e) => {
            tracing::warn!(error = %e, "pull: listing volumes; this beat does nothing");
            return None;
        }
    };
    let replicas = match Api::<crd::VolumeReplica>::all(ctx.client.clone()).list(&ListParams::default()).await {
        Ok(l) => l.items,
        Err(e) => {
            tracing::warn!(error = %e, "pull: listing replicas; this beat does nothing");
            return None;
        }
    };
    Some(Beat { volumes, replicas, parents: parents_on_node(ctx).await? })
}
```

  In `bins/agent/src/lib.rs`, add `pub mod listing;` beside the other module declarations.

- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-git-agent-bin && cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] **Step 5: Commit** — `git add bins/agent/src/listing.rs bins/agent/src/lib.rs && git commit -m "Add the one per-beat listing of this node's parents and the cluster's volumes"`

---

### Task 6: Thread the shared listing through the pull beat

Detail finding 7 / summary High #6, the consumer half. `pull_beat_with` (`peer.rs:319`) currently lists: Nodes ×1 (`:324`), VolumeReplicas ×1 (`:703`), Workspaces ×1 + Environments ×1 (`:794`), Volumes ×1 (`:876`), Volumes ×1 + Workspaces ×1 + Environments ×1 (`:376/399/412`), Volumes ×1 + VolumeReplicas ×1 + Workspaces ×1 + Environments ×1 (`:995/1002/960/967`).

**Files:** Modify `bins/agent/src/peer.rs` — `pull_beat_with` (`:319-362`), `interesting_volumes` (`:373-427`), `reap_dead_replicas` (`:701-720`), `unclaim_dead_nodes` (`:728-764`), `release_dead_volumes` (`:874-...`), `hosted_volumes` (`:955-980`, deleted), `retire_pass` (`:952-...`).

**Interfaces:**
- Consumes `listing::beat(&Arc<Ctx>) -> Option<listing::Beat>` and `Beat::hosted_volumes`.
- Produces `async fn pull_beat_with(ctx: &Arc<Ctx>, btrfs_bin: &str, secret: &str)` (unchanged signature) and the narrowed helpers:
  - `fn interesting_volumes(ctx: &Arc<Ctx>, beat: &Beat, live: &[String]) -> Vec<String>` (now sync, no I/O)
  - `async fn reap_dead_replicas(ctx: &Arc<Ctx>, beat: &Beat, nodes: &[Node], floor: i64, now: Timestamp)`
  - `async fn unclaim_dead_nodes(ctx: &Arc<Ctx>, beat: &Beat, nodes: &[Node], floor: i64, now: Timestamp)`
  - `async fn release_dead_volumes(ctx: &Arc<Ctx>, beat: &Beat, nodes: &[Node], floor: i64, now: Timestamp, running: &HashSet<String>)`
  - `async fn retire_pass(ctx: &Arc<Ctx>, beat: &Beat, live: &[String])`
  - `hosted_volumes` is DELETED — `Beat::hosted_volumes()` replaces it.
- `unclaim_kind` keeps its own `Api::list` (it needs the whole cluster's parents, not this node's) — the report's finding 13 dedup of its four closures is deferred (see Self-review).

- [ ] **Step 1: Write the failing test** — append to `bins/agent/src/peer.rs`'s `mod reconcile_tests`:

```rust
    /// The listing budget: one pull beat over one volume makes ONE Volume list, ONE VolumeReplica
    /// list for the beat, ONE Workspace list and ONE Environment list for this node's parents —
    /// plus `unclaim_kind`'s cluster-wide pair and the per-volume snapshot list. What it must never
    /// do again is re-list Volumes three times and Workspaces/Environments three times.
    #[tokio::test]
    async fn a_pull_beat_lists_each_kind_once_for_the_beat() {
        let tmp = tempfile::tempdir().unwrap();
        let volume = serde_json::json!({
            "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "Volume",
            "metadata": {"name": "v1"},
            "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 5, "replicas": 1},
            "status": {"phase": "ready"},
        });
        let routes = vec![
            Route { method: "GET", path: NODES.into(), status: 200, body: list_of("Node", vec![node_json("node-a", "True", "2000-01-01T00:00:00Z")]) },
            Route { method: "GET", path: VOLUMES.into(), status: 200, body: list_of("Volume", vec![volume]) },
            Route { method: "GET", path: VOLREPLICAS.into(), status: 200, body: list_of("VolumeReplica", vec![]) },
            Route { method: "GET", path: WORKSPACES.into(), status: 200, body: list_of("Workspace", vec![]) },
            Route { method: "GET", path: ENVIRONMENTS.into(), status: 200, body: list_of("Environment", vec![]) },
            Route { method: "GET", path: SNAPSHOTS.into(), status: 200, body: list_of("Snapshot", vec![]) },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        pull_beat_with(&ctx, "btrfs", "s3cret").await;

        let count = |p: &str| rec.calls().iter().filter(|c| c.as_str() == format!("GET {p}")).count();
        assert_eq!(count(VOLUMES), 1, "{:?}", rec.calls());
        assert_eq!(count(VOLREPLICAS), 1, "{:?}", rec.calls());
        assert!(count(WORKSPACES) <= 2, "{:?}", rec.calls());
        assert!(count(ENVIRONMENTS) <= 2, "{:?}", rec.calls());
    }
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-git-agent-bin a_pull_beat_lists_each_kind_once` fails with `assertion 'left == right' failed: left: 3, right: 1` on `VOLUMES`.

- [ ] **Step 3: Implement** — in `bins/agent/src/peer.rs`:

  Replace the body of `pull_beat_with` from `:352` (`let live = live_nodes(...)`) upward so the beat is listed once, right after `nodes`/`now`/`floor`:

```rust
    // One clock and one floor for the whole pass: reap, unclaim and live_nodes must agree on
    // exactly the same "dead" answer, not three readings a few nanoseconds apart.
    let now = k8s_openapi::jiff::Timestamp::now();
    let floor = node_dead_secs();

    // And one LISTING for the whole pass, for the same reason the node list is threaded: reap,
    // unclaim, place and retire each decide what to delete, and two of them acting on different
    // views of the cluster is how a copy nobody else holds gets dropped.
    let Some(beat) = crate::listing::beat(ctx).await else { return };

    reap_dead_replicas(ctx, &beat, &nodes, floor, now).await;
    unclaim_dead_nodes(ctx, &beat, &nodes, floor, now).await;

    let candidates = match pool_nodes(&ctx.client).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "pull: listing pool nodes");
            return;
        }
    };

    let http = match peer_http_client() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "pull: building the http client");
            return;
        }
    };

    let live = live_nodes(&candidates, &nodes, floor, now);
    for id in interesting_volumes(ctx, &beat, &live) {
        pull_volume(ctx, btrfs_bin, &http, secret, &id).await;
    }
    retire_pass(ctx, &beat, &live).await;
```

  `interesting_volumes` (`:373`) loses both of its own listings and becomes synchronous:

```rust
fn interesting_volumes(ctx: &Arc<Ctx>, beat: &crate::listing::Beat, live: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for v in &beat.volumes {
        if v.metadata.deletion_timestamp.is_some() {
            continue;
        }
        let id = v.name_any();
        let i_am_owner = v.spec.node_name == ctx.node;
        let owner_alive = live.iter().any(|n| n == &v.spec.node_name);
        let targets = replicate::targets(&id, &v.spec.node_name, live, standby_count(owner_alive, v.spec.replicas));
        // Holding a copy on disk is interesting on its own: with `replicas: 1` a returning node's
        // replica row was reaped while it was dead and rendezvous elected someone else who has no
        // source at all, so nothing would ever re-register the one copy that exists.
        let hold_a_copy = ctx.engine.pool.voldir(&id).exists();
        if (i_am_owner || hold_a_copy || targets.iter().any(|t| t == &ctx.node)) && !out.contains(&id) {
            out.push(id);
        }
    }
    // The parent half: a worktree running here needs its volume pulled whether or not rendezvous
    // named this node. Same list `retire_pass` and the sync beat read.
    for p in &beat.parents {
        if !out.contains(&p.volume) {
            out.push(p.volume.clone());
        }
    }
    out
}
```

  `reap_dead_replicas` (`:701`) drops its list and takes `beat.replicas`:

```rust
async fn reap_dead_replicas(ctx: &Arc<Ctx>, beat: &crate::listing::Beat, nodes: &[Node], floor: i64, now: k8s_openapi::jiff::Timestamp) {
    let replica_api: Api<crd::VolumeReplica> = Api::all(ctx.client.clone());
    for r in &beat.replicas {
        if node_is_dead(nodes.iter().find(|n| n.name_any() == r.spec.node), floor, now) {
            let rname = r.name_any();
            if let Err(e) = replica_api.delete(&rname, &Default::default()).await {
                tracing::warn!(replica = %rname, error = %e, "pull: reaper: deleting a dead node's replica row");
            }
        }
    }
}
```

  `unclaim_dead_nodes` (`:728`) takes `beat` and passes it to `release_dead_volumes` only (its own two `unclaim_kind` calls stay cluster-wide — a DEAD node's parents are by definition not on this node, so `beat.parents` cannot see them):

```rust
async fn unclaim_dead_nodes(ctx: &Arc<Ctx>, beat: &crate::listing::Beat, nodes: &[Node], floor: i64, now: k8s_openapi::jiff::Timestamp) {
```

  …and its final block:

```rust
    if ws_ok && envs_ok {
        release_dead_volumes(ctx, beat, nodes, floor, now, &running_volumes).await;
    }
```

  `release_dead_volumes` (`:874`) drops its list and its `ponytail:` note (the duplicate is gone):

```rust
async fn release_dead_volumes(
    ctx: &Arc<Ctx>,
    beat: &crate::listing::Beat,
    nodes: &[Node],
    floor: i64,
    now: k8s_openapi::jiff::Timestamp,
    running: &HashSet<String>,
) {
    let api: Api<crd::Volume> = Api::all(ctx.client.clone());
    for vol in beat.volumes.iter().cloned() {
```

  Delete `hosted_volumes` (`:955-980`) entirely. `retire_pass` (`:952`) drops its three listings:

```rust
async fn retire_pass(ctx: &Arc<Ctx>, beat: &crate::listing::Beat, live: &[String]) {
    let vols = &beat.volumes;
    let rows = &beat.replicas;
    let hosted = beat.hosted_volumes();
```

  (the remainder of `retire_pass` is unchanged; `vols`/`rows` are now slices, so `for v in vols` and `rows.iter()` replace the owned iterations.)

- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-git-agent-bin && cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] **Step 5: Commit** — `git add bins/agent/src/peer.rs && git commit -m "Thread one listing through the pull beat instead of re-listing every kind"`

---

### Task 7: Resolve a source's peer address once per `pull_volume`

Detail finding 8, summary High #6's second half. `peer.rs:532` calls `agent_pod_addr` (`:235`, a namespaced pod LIST with two selectors) inside `for name in order { for &source in &sources {` — a node catching up on N missing commits makes N × sources identical lists to learn the same peer IP.

**Files:** Modify `bins/agent/src/peer.rs:508-560` (`pull_volume`'s source setup and the pull loop).

**Interfaces:** Consumes `agent_pod_addr(client: &kube::Client, node: &str) -> Result<String, String>` (unchanged). Produces a local `let addrs: Vec<(&str, String)>` resolved before the loop.

- [ ] **Step 1: Write the failing test** — append to `mod reconcile_tests` in `peer.rs`:

```rust
    /// Catching up on three commits from one source resolves that source's pod address ONCE, not
    /// once per commit: a full namespaced pod list with two selectors is not a per-commit cost.
    #[tokio::test]
    async fn pull_volume_resolves_a_source_address_once_per_pass() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("vol/v1/snap")).unwrap();
        let snaps = vec![
            ready_snapshot("v1-aaaa", "v1", ""),
            ready_snapshot("v1-bbbb", "v1", "v1-aaaa"),
            ready_snapshot("v1-cccc", "v1", "v1-bbbb"),
        ];
        let replica_b = serde_json::json!({
            "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "VolumeReplica",
            "metadata": {"name": "v1.node-b", "uid": "uid-b"},
            "spec": {"volume": "v1", "node": "node-b"},
            "status": {"phase": "Synced", "branches": {}},
        });
        let routes = vec![
            Route { method: "GET", path: SNAPSHOTS.into(), status: 200, body: list_of("Snapshot", snaps) },
            Route { method: "GET", path: VOLREPLICAS.into(), status: 200, body: list_of("VolumeReplica", vec![replica_b]) },
            Route { method: "GET", path: "/api/v1/namespaces/kube-system/pods".into(), status: 200, body: list_of("Pod", vec![]) },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);
        let http = peer_http_client().unwrap();

        pull_volume(&ctx, "btrfs", &http, "s3cret", "v1").await;

        let pod_lists = rec.calls().iter().filter(|c| c.as_str() == "GET /api/v1/namespaces/kube-system/pods").count();
        assert_eq!(pod_lists, 1, "one address lookup per source per pass, not per commit: {:?}", rec.calls());
    }
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-git-agent-bin pull_volume_resolves_a_source_address_once` fails with `assertion 'left == right' failed: left: 3, right: 1`.

- [ ] **Step 3: Implement** — after the `sources` vector is built (`peer.rs:516`), insert:

```rust
    // Resolved ONCE per pass, before the commit loop: `agent_pod_addr` is a namespaced pod LIST
    // with two selectors, and a node catching up on N commits was making N of them per source to
    // learn the same IP. A source whose pod cannot be found now is skipped for the whole pass —
    // which is what the per-commit `continue` amounted to anyway, one list at a time.
    let mut addrs: Vec<(&str, String)> = Vec::new();
    for &source in &sources {
        match agent_pod_addr(&ctx.client, source).await {
            Ok(a) => addrs.push((source, a)),
            Err(e) => tracing::warn!(%volume, source, error = %e, "pull: no peer address; skipping this source"),
        }
    }
```

  and replace the inner loop head (`:531-540`):

```rust
        for (source, addr) in &addrs {
            let source = *source;
```

  (delete the per-iteration `let addr = match agent_pod_addr(...)` block; `pull_one`'s call becomes `…, addr, secret, …` since `addr` is now a `&String`.)

- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-git-agent-bin && cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] **Step 5: Commit** — `git add bins/agent/src/peer.rs && git commit -m "Resolve each pull source's peer address once per volume, not per commit"`

---

### Task 8: Move the sync beat onto the shared listing

Detail finding 13's third bullet. `sync::live_worktrees` (`sync.rs:69-105`) is the third verbatim copy of the parents-on-this-node query.

**Files:** Modify `bins/agent/src/sync.rs:55-105` (`sync_beat` and `live_worktrees`, the latter deleted). Test in `bins/agent/src/sync.rs`'s existing `mod tests` (`:187`).

**Interfaces:** Consumes `listing::parents_on_node` and `Parent::is_live_worktree`. `Live` (the local struct) is replaced by `listing::Parent`; `sync_one(ctx: &Arc<Ctx>, live: &listing::Parent)` keeps its shape, reading `p.volume`, `p.name` (as the worktree), `p.owner` and `p.owner_ref`.

- [ ] **Step 1: Write the failing test** — append to `sync.rs`'s `mod tests`:

```rust
    /// The sync beat reads this node's parents through the shared listing — and a listing that
    /// could not be completed cuts nothing, rather than treating an empty view as "no worktrees".
    #[tokio::test]
    async fn a_failed_parent_listing_cuts_no_sync_points() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![Route {
            method: "GET",
            path: "/apis/kloudlite-git.io/v1alpha1/workspaces".into(),
            status: 500,
            body: serde_json::json!({}),
        }];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);
        sync_beat(&ctx).await;
        assert!(rec.calls().iter().all(|c| !c.starts_with("POST")), "{:?}", rec.calls());
    }
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-git-agent-bin a_failed_parent_listing_cuts_no_sync_points` fails to compile: `error[E0425]: cannot find function 'test_ctx' in this scope` until the harness is copied from `peer.rs:1079-1096`, then fails on the `sync_beat` signature.

- [ ] **Step 3: Implement** — replace `sync.rs:55-105`:

```rust
pub async fn sync_beat(ctx: &Arc<Ctx>) {
    // Keep-biased like every other beat: a half-listed cluster cuts nothing. A missed sync point
    // costs one `WS_SYNC_SECS` of freshness on a replica; acting on a partial view costs more.
    let Some(parents) = crate::listing::parents_on_node(ctx).await else { return };
    for p in parents.iter().filter(|p| p.is_live_worktree()) {
        sync_one(ctx, p).await;
    }
}
```

  Delete `live_worktrees` and the local `struct Live`; change `sync_one`'s signature to `async fn sync_one(ctx: &Arc<Ctx>, live: &crate::listing::Parent)` — every field it reads (`live.volume`, `live.worktree` → `live.name`, `live.owner`, `live.owner_ref`) keeps its meaning, so only `live.worktree` is renamed to `live.name` throughout the body.

- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-git-agent-bin && cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] **Step 5: Commit** — `git add bins/agent/src/sync.rs && git commit -m "Read the sync beat's worktrees from the shared parent listing"`

---

### Task 9: Scope retention's parent scans

Detail finding 10, summary Medium. `snapshot::worktree_heads` (`:215` and `:231`) lists ALL Workspaces and ALL Environments unscoped, on every non-transient cut — once per user push — and filters by name prefix client-side.

**Note on the review's suggested fix:** `crd::VOLUME_LABEL` is stamped by `commit_labels` on *Snapshots*, not on parents, so it cannot select parents. This task uses the OWNER label instead (`k8s::OWNER_LABEL`, re-stamped by `heal_labels` on every reconcile) — a volume's worktrees all belong to one owner, and the owner is already in hand from the snapshot list `retain` has just made. Node scoping is deliberately NOT used: a head must stay protected even if its worktree is claimed elsewhere mid-takeover.

**Files:** Modify `bins/agent/src/snapshot.rs:203-244` (`worktree_heads`) and `:302-309` (its call site in `retain`).

**Interfaces:** Produces `async fn worktree_heads(ctx: &Arc<Ctx>, volume: &str, owner: &str) -> Result<HashSet<String>, ReconcileErr>` (was without `owner`). Consumed by `retain` only.

- [ ] **Step 1: Write the failing test** — append to `snapshot.rs`'s test module (copy `peer.rs:1079-1096`'s `test_ctx` if the module has none):

```rust
    /// Retention's head scan is owner-scoped: an unfiltered cluster-wide Workspace and Environment
    /// list on every push is a full scan for a prefix match it does client-side anyway.
    #[tokio::test]
    async fn worktree_heads_selects_by_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![
            Route { method: "GET", path: "/apis/kloudlite-git.io/v1alpha1/workspaces".into(), status: 200, body: list_of("Workspace", vec![]) },
            Route { method: "GET", path: "/apis/kloudlite-git.io/v1alpha1/environments".into(), status: 200, body: list_of("Environment", vec![]) },
        ];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        worktree_heads(&ctx, "v1", "alice").await.unwrap();

        for kind in ["workspaces", "environments"] {
            let req = rec.requests().into_iter().find(|r| r.contains(&format!("/{kind}?"))).unwrap_or_default();
            assert!(req.contains("owner%3Dalice") || req.contains("owner=alice"), "{kind}: {req}");
        }
    }
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-git-agent-bin worktree_heads_selects_by_owner` fails with `workspaces: ` (an empty match — the request carries no `labelSelector`).

- [ ] **Step 3: Implement** — in `snapshot.rs`, change the signature and both listings:

```rust
async fn worktree_heads(ctx: &Arc<Ctx>, volume: &str, owner: &str) -> Result<std::collections::HashSet<String>, ReconcileErr> {
    // F4: matched on the commit NAME's `{volume}-` prefix (`crd::snapshot_name`), not on
    // `volume_ref` — a worktree whose status is mid-rebuild has `volumeRef` momentarily unset,
    // and filtering on it there would make its head briefly invisible to retention.
    //
    // Owner-scoped server-side: this runs on every push, and the unscoped version was a full
    // cluster scan of both parent kinds each time. A volume's worktrees all belong to one owner,
    // and the owner comes from the snapshot list the caller already has. NOT node-scoped: a head
    // must stay protected while its worktree is claimed on another node mid-takeover.
    let mine = ListParams::default().labels(&format!("{}={owner}", kloudlite_git_workspaces::k8s::OWNER_LABEL));
    let prefix = format!("{volume}-");
    let mut heads = std::collections::HashSet::new();
    for w in Api::<crd::Workspace>::all(ctx.client.clone()).list(&mine).await?.items {
```

  …and the same `&mine` on the `Environment` list at `:231`. At the call site (`:304`):

```rust
    // The owner comes from the volume's own commits — every `Snapshot` on this volume carries it,
    // and `retain` has already listed them.
    let owner = by_name.values().next().map(|s| s.spec.owner.clone()).unwrap_or_default();
    let heads = match worktree_heads(ctx, volume, &owner).await {
```

- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-git-agent-bin && cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] **Step 5: Commit** — `git add bins/agent/src/snapshot.rs && git commit -m "Scope retention's worktree head scan to the volume's owner"`

---

### Task 10: Extract `controller/stop.rs`

Detail finding 12 / Architecture #2, first seam. Pure move — the shared, kind-agnostic half of both parent kinds, and the most subtle code in the file.

**Files:**
- Create `bins/agent/src/controller/stop.rs` from `bins/agent/src/controller.rs:2824-3022`: `STOP_GENERATION` (`:2846`), `StopPush` (`:2828-2838`), `stop_name` (`:2864`), `stop_push` (`:2868-2956`), `flush_timeout` (`:2958`), `flush_expired` (`:2967`), `flush_gate` (`:2977-3014`), and the three `FlushUnreplicated` constants `NO_PEERS`/`NO_READY_AT`/`FLUSH_TIMED_OUT` (`:3018-3020`).
- Modify `bins/agent/src/controller.rs` — becomes `bins/agent/src/controller/mod.rs` in Task 12; for this task keep `controller.rs` and add `#[path = "controller/stop.rs"] pub(crate) mod stop;` at the top, plus `pub(crate) use stop::{flush_timeout, stop_name, stop_push, StopPush};`.

**Interfaces:** Produces `pub(crate) mod stop` exporting `StopPush`, `stop_name`, `stop_push`, `flush_timeout`. Consumes `Ctx`, `ReconcileErr`, `crd`, `crate::peer::pool_nodes` — all imported at the top of the new file. Consumed unchanged by `stop_workspace` (`:1767`) and `stop_environment` (`:2473`).

- [ ] **Step 1: Write the failing test** — no new test: a pure move is verified by the EXISTING suite being unchanged. Record the baseline first: `cargo test -p kloudlite-git-agent-bin 2>&1 | tail -20 > /tmp/before.txt`.
- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-git-agent-bin` must pass at HEAD before the move (this is the baseline, not a red test); after creating the file and before adding the `mod` line it fails with `error[E0433]: failed to resolve: use of undeclared crate or module 'stop'`.
- [ ] **Step 3: Implement** — `git mv`-equivalent by hand: create `bins/agent/src/controller/stop.rs` with this header, then move the eight items verbatim (no edits to their bodies beyond `pub(crate)` on the items `controller.rs` still calls):

```rust
//! The stop-before-teardown gate both parent kinds share: cut a final sync point, wait until
//! another node's replica reports `Synced` at or after it, then let the caller tear down.
//!
//! Kind-agnostic on purpose — a `Workspace` and an `Environment` share no status type, and this is
//! the one place a stop request's lifecycle is decided. Split out of `controller.rs` unchanged.

use super::{Ctx, ReconcileErr};
use kube::{Api, ResourceExt};
use kloudlite_git_workspaces::crd;
use std::sync::Arc;
use std::time::Duration;
```

  Add to `bins/agent/src/controller.rs`, immediately after its `use` block:

```rust
#[path = "controller/stop.rs"]
pub(crate) mod stop;
pub(crate) use stop::{flush_timeout, stop_name, stop_push, StopPush};
```

- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-git-agent-bin 2>&1 | tail -20 > /tmp/after.txt && diff /tmp/before.txt /tmp/after.txt` (must be empty), then `git diff --stat` (must show only `controller.rs` deletions and `controller/stop.rs` additions), then `cargo clippy --workspace --all-targets --locked -- -D warnings`.
- [ ] **Step 5: Commit** — `git add bins/agent/src/controller.rs bins/agent/src/controller/stop.rs && git commit -m "Move the shared stop gate into controller/stop.rs"`

---

### Task 11: Extract `controller/status.rs`

Second seam. Pure move.

**Files:** Create `bins/agent/src/controller/status.rs` from `controller.rs`: `write_status` (`:905-940`), `settled_status_eq` (`:941-978`), `replace_status` (`:979-999`), `Outcome` + its `From<kube::Error>` (`:1000-1023`), `settle` (`:1024-1056`), `patch_status` (`:1057-1078`), `conditions_eq` (`:927`), and the shared-plumbing tail `applied_key` (`:3110`), `forget_applied` (`:3117`), `ensure`, `create_if_absent` (`:3134`), `delete_ignoring_404` (`:3153`). Modify `controller.rs` (add the `mod` + re-export).

**Interfaces:** Produces `pub(crate) mod status` re-exported flat into `controller` so no call site changes: `pub(crate) use status::{conditions_eq, create_if_absent, delete_ignoring_404, ensure, forget_applied, patch_status, replace_status, settle, write_status, Outcome};`.

- [ ] **Step 1: Write the failing test** — none; pure move. Baseline: `cargo test -p kloudlite-git-agent-bin 2>&1 | tail -20 > /tmp/before.txt`.
- [ ] **Step 2: Run it, expect failure** — after creating the file and deleting the originals, before the `mod`/`use` lines: `error[E0425]: cannot find function 'settle' in this scope`.
- [ ] **Step 3: Implement** — the new file's header:

```rust
//! Every status write in this controller, and the object-applied bookkeeping around them.
//!
//! One module because they share one invariant: a status write that produces new bytes fires this
//! controller's own watch, which writes again — `settled_status_eq` and `conditions_eq` are what
//! make a converged pass idle instead of hot-looping. Split out of `controller.rs` unchanged.

use super::{Ctx, ReconcileErr};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::api::{Patch, PatchParams, PostParams};
use kube::runtime::controller::Action;
use kube::{Api, Resource, ResourceExt};
use kloudlite_git_workspaces::crd;
use std::sync::Arc;
```

  plus the `mod`/`use` pair in `controller.rs`, matching Task 10's shape.

- [ ] **Step 4: Run tests and clippy** — same three checks as Task 10 (`diff /tmp/before.txt /tmp/after.txt` empty, `git diff --stat` moves only, clippy clean).
- [ ] **Step 5: Commit** — `git add bins/agent/src/controller.rs bins/agent/src/controller/status.rs && git commit -m "Move the status writes into controller/status.rs"`

---

### Task 12: Turn `controller.rs` into `controller/mod.rs` and extract `controller/run.rs`

Third and fourth seams. Pure move.

**Files:**
- `git mv bins/agent/src/controller.rs bins/agent/src/controller/mod.rs`; drop the two `#[path = …]` attributes added in Tasks 10-11 (plain `mod stop; mod status;` now).
- Create `bins/agent/src/controller/run.rs` from `mod.rs`: `owned_by` (`:222-238`), `wake_stream` (`:239-247`), `timed` (`:248-256`), `run` (`:257-463`), `shutdown_signal` (`:464-473`), `error_policy` (`:474-487`), `heartbeat` (`:488-501`), `spawn_heartbeat` (`:502-517`), `spawn_pull` (`:518-530`), `spawn_sync` (`:531-547`), `wake_on_finish` (`:548-560`), `running_contains` (`:561-575`).
- `mod.rs` keeps: the module doc (`:1-13`), the `use` block, `TICK` (`:37`), `RETRY` (`:41`), `InFlight` (`:49`), `API_SERVICE_ACCOUNT`/`API_NAMESPACE` (`:53-54`), `Ctx` + its two `impl`s (`:56-186`), `Done` (`:187-199`), `ReconcileErr` + its impls (`:200-221`), `Work` (`:777-787`), and the `mod`/`pub use` lines.

**Interfaces:** Produces `pub mod run` re-exported as `pub use run::{run, running_contains, wake_on_finish};` — `bins/agent/src/main.rs` calls `controller::run`, so the re-export keeps its path.

- [ ] **Step 1: Write the failing test** — none; pure move. Baseline `/tmp/before.txt` as before.
- [ ] **Step 2: Run it, expect failure** — `error[E0432]: unresolved import 'kloudlite_git_agent::controller::run'` from `main.rs` until the re-export lands.
- [ ] **Step 3: Implement** — `run.rs`'s header:

```rust
//! Starting the controller: the three `Controller` builders, the watches they open, the background
//! beats they spawn, and the error policy. Nothing here decides anything about a workspace — this
//! is the wiring the reconcilers hang off. Split out of `controller.rs` unchanged.

use super::{Ctx, InFlight, ReconcileErr, RETRY, TICK};
use futures::StreamExt;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher;
use kube::{Api, Resource, ResourceExt};
use kloudlite_git_workspaces::crd;
use std::sync::Arc;
use std::time::Duration;
```

  and in `mod.rs`: `mod run; mod stop; mod status; pub use run::{run, running_contains, wake_on_finish};` alongside the Task 10/11 re-exports.

- [ ] **Step 4: Run tests and clippy** — same three checks as Task 10.
- [ ] **Step 5: Commit** — `git add -A bins/agent/src/controller* && git commit -m "Split the controller's startup wiring into controller/run.rs"`

---

### Task 13: Extract `controller/volume.rs`

Fifth seam. Pure move.

**Files:** Create `bins/agent/src/controller/volume.rs` from `mod.rs`: `heal_labels` (`:576-593`), `owner_ref_of_kind` (`:594-599`), `reconcile_volume` (`:600-615`), `apply_volume` (`:616-757`), `permanent_reason` (`:758-765`), `progressing` (`:766-776`), `volume_work` (`:788-841`), `cleanup_volume` (`:842-884`), `write_volume_status` (`:885-904`), `ensure_child_volume` (`:1079-1162`), `check_source` (`:1163-1190`), `Resolved` (`:1191-1207`), `resolve_volume` (`:1208-1342`).

`heal_labels` and `owner_ref_of_kind` move here because `resolve_volume` and both parent files call them; they are re-exported (`pub(crate)`) from `mod.rs`. Note `listing.rs` (Task 5) calls `controller::owner_ref_of_kind` — the re-export keeps that path.

**Interfaces:** Produces `pub(crate) mod volume` and `pub(crate) use volume::{apply_volume, cleanup_volume, heal_labels, owner_ref_of_kind, reconcile_volume, resolve_volume, Resolved};` plus `pub use volume::apply_volume;` (the integration tests in `bins/agent/tests/reconcile.rs` call `kloudlite_git_agent::controller::apply_volume`).

- [ ] **Step 1: Write the failing test** — none; pure move. Baseline `/tmp/before.txt`.
- [ ] **Step 2: Run it, expect failure** — `error[E0425]: cannot find function 'apply_volume' in module 'kloudlite_git_agent::controller'` from `tests/reconcile.rs` until the re-export lands.
- [ ] **Step 3: Implement** — header:

```rust
//! The `Volume` reconciler: the one CRD whose node lives in SPEC, and the only place btrfs work is
//! started. `resolve_volume` and `ensure_child_volume` are the parents' shared entry point into it
//! — a workspace and an environment both own exactly one volume with identical semantics.
//! Split out of `controller.rs` unchanged.

use super::{Ctx, Done, InFlight, Outcome, ReconcileErr, Work, RETRY, TICK};
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{finalizer, Event as FinalizerEvent};
use kube::{Api, Resource, ResourceExt};
use kloudlite_git_workspaces::crd::{self, Phase, VolumeSource};
use kloudlite_git_workspaces::engine::Engine;
use std::sync::Arc;
```

- [ ] **Step 4: Run tests and clippy** — same three checks as Task 10.
- [ ] **Step 5: Commit** — `git add -A bins/agent/src/controller && git commit -m "Split the volume reconciler into controller/volume.rs"`

---

### Task 14: Extract `controller/workspace.rs`

Sixth seam. Pure move.

**Files:** Create `bins/agent/src/controller/workspace.rs` from `mod.rs:1344-2362`: `profile_failed` (`:1347`), `ensure_profile` (`:1373`), `build_failed_backoff` (`:1565`), `packages_status` (`:1578`), `ensure_ssh` (`:1602`), `write_ws_status_tracking` (`:1662`), `write_resolv_conf` (`:1685`), `ws_conditions` (`:1707`), `kept_conditions` (`:1718`), `with_attached` (`:1727`), `stop_workspace` (`:1734`), `is_shared_clone` (`:1807`), `has_worktree_finalizer` (`:1815`), `reconcile_workspace` (`:1823`), `cleanup_workspace_worktree` (`:1840`), `migrate_and_seed_baseline` (`:1868`), `ensure_shared_home` (`:1910`), `apply_workspace` (`:1928`), `pod_carries_the_attach_mount` (`:2325`), `pod_is_ready` (`:2334`), `write_ws_status` (`:2344`).

**Interfaces:** Produces `pub(crate) mod workspace` and re-exports the names other modules and tests use: `pub use workspace::{apply_workspace, cleanup_workspace_worktree, reconcile_workspace};` and `pub(crate) use workspace::{kept_conditions, write_resolv_conf, write_ws_status};` (`snapshot.rs:190` calls `write_ws_status`; `peer.rs:747` calls `kept_conditions`).

- [ ] **Step 1: Write the failing test** — none; pure move. Baseline `/tmp/before.txt`.
- [ ] **Step 2: Run it, expect failure** — `error[E0603]: function 'write_ws_status' is private` from `snapshot.rs` until the re-export lands.
- [ ] **Step 3: Implement** — header:

```rust
//! The `Workspace` reconciler: profile, host key, home, worktree, attachment and the one pod.
//! Split out of `controller.rs` unchanged.

use super::stop::{stop_name, stop_push, StopPush};
use super::{Ctx, Outcome, ReconcileErr, RETRY, TICK};
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, OwnerReference};
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{finalizer, Event as FinalizerEvent};
use kube::{Api, Resource, ResourceExt};
use kloudlite_git_workspaces::crd::{self, DesiredState, VolumeSource};
use kloudlite_git_workspaces::k8s;
use kloudlite_git_workspaces::model;
use std::sync::Arc;
```

- [ ] **Step 4: Run tests and clippy** — same three checks as Task 10.
- [ ] **Step 5: Commit** — `git add -A bins/agent/src/controller && git commit -m "Split the workspace reconciler into controller/workspace.rs"`

---

### Task 15: Extract `controller/environment.rs`

Seventh seam. Pure move; after it, `mod.rs` holds only `Ctx`, `Done`, `Work`, `ReconcileErr`, the constants and the module wiring.

**Files:** Create `bins/agent/src/controller/environment.rs` from `mod.rs:2364-2824` and `:3024-3058`: `apply_environment` (`:2364`), `stop_environment` (`:2424`), `stopped_condition` (`:2508`), `run_environment` (`:2518`), `deployment_status` (`:2670`), `writing_pods` (`:2686`), `restore_gate` (`:2712`), `drain_services` (`:2794`), `mkdir_env_mounts` (`:3024`), `write_env_status` (`:3035`). Also move `reconcile_environment` and `cleanup_environment` if present in that range.

**Interfaces:** Produces `pub(crate) mod environment` and `pub use environment::{apply_environment, reconcile_environment};` plus `pub(crate) use environment::write_env_status;` (`snapshot.rs:196` calls it).

- [ ] **Step 1: Write the failing test** — none; pure move. Baseline `/tmp/before.txt`.
- [ ] **Step 2: Run it, expect failure** — `error[E0425]: cannot find function 'write_env_status' in this scope` from `snapshot.rs`.
- [ ] **Step 3: Implement** — header:

```rust
//! The `Environment` reconciler: one volume, a namespace of StatefulSets, and the restore gate.
//! Split out of `controller.rs` unchanged.

use super::stop::{stop_name, stop_push, StopPush};
use super::{Ctx, Outcome, ReconcileErr, RETRY, TICK};
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{LimitRange, Namespace, Pod, Service};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{finalizer, Event as FinalizerEvent};
use kube::{Api, Resource, ResourceExt};
use kloudlite_git_workspaces::crd::{self, DesiredState};
use kloudlite_git_workspaces::k8s;
use kloudlite_git_workspaces::model;
use std::sync::Arc;
```

- [ ] **Step 4: Run tests and clippy** — same three checks as Task 10, plus `wc -l bins/agent/src/controller/*.rs` (no file over ~1100 lines; `mod.rs` under 300).
- [ ] **Step 5: Commit** — `git add -A bins/agent/src/controller && git commit -m "Split the environment reconciler into controller/environment.rs"`

---

### Task 16: Delete the dead code and the phantom env var

Detail finding 14, summary Lows. `Done::lineage_tip` is written `None` at its only producer and read nowhere (`grep -rn lineage_tip bins crates` returns exactly two hits: `controller.rs:189` and `:835`). `peer.rs:248`'s doc comment names `WS_PEER_RECV_TIMEOUT_SECS`, which exists nowhere in the repo.

**Files:** Modify `bins/agent/src/controller/mod.rs` (the `Done` struct, ex-`controller.rs:189`), `bins/agent/src/controller/volume.rs` (ex-`:835`), `bins/agent/src/peer.rs:246-249`.

**Interfaces:** Produces `pub struct Done { pub phase: Phase, pub restored_to: Option<String>, pub quota_unenforced: Option<String> }` — one field fewer. `bins/agent/tests/reconcile.rs` constructs `Done` with `..Done::default()`, so no test changes.

- [ ] **Step 1: Write the failing test** — none: deleting a field the compiler proves unread is verified by the build. Confirm the premise first: `grep -rn 'lineage_tip\|WS_PEER_RECV_TIMEOUT_SECS' bins crates deploy` must return exactly the three lines named above.
- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-git-agent-bin` passes at HEAD; after deleting only the struct field it fails with `error[E0560]: struct 'Done' has no field named 'lineage_tip'` at the producer, which is the second edit.
- [ ] **Step 3: Implement** — delete `controller/mod.rs`'s `pub lineage_tip: Option<String>,` and `controller/volume.rs`'s `lineage_tip: None,`. In `peer.rs`, fix the `send_timeout` doc:

```rust
/// `WS_PEER_SEND_TIMEOUT_SECS`, default 3600. A send is legitimately tens of GiB; this exists to
/// unwedge a connection that has actually stalled, not to police link speed. The receive side has
/// no timeout knob of its own — the sender's is the only bound on a transfer.
```

- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-git-agent-bin && cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] **Step 5: Commit** — `git add bins/agent/src && git commit -m "Delete the unread lineage tip and a doc reference to a variable that never existed"`

---

### Task 17: Make the `NamespaceReady` gate answer for this node

Detail finding 5, summary Low. `teams_in_use` (`binding.rs:44`) is scoped to THIS node's workspaces; `write_binding_status` (`:66`) writes one `NamespaceReady=True` on the single cluster-scoped `OwnerBinding`; `namespace_ready` (`:140`) is read by every node. A workspace claimed on node B in a team A has never seen passes the gate before B has applied that team's namespace, and `ensure_ssh` then fails into the 60 s `RETRY`.

The lazy fix is the second option the review names: have `namespace_ready` verify the specific namespace exists, instead of trusting a cluster-wide condition to describe a per-node fact.

**Files:** Modify `bins/agent/src/binding.rs:138-145` (`namespace_ready`) and its call site `bins/agent/src/controller/workspace.rs` (ex-`controller.rs:1976`) to pass the team.

**Interfaces:** Produces `pub async fn namespace_ready(ctx: &Arc<Ctx>, region: &str, owner: &str, team: &str) -> Result<bool, ReconcileErr>` (gains `team`). Consumed by `apply_workspace` only.

- [ ] **Step 1: Write the failing test** — append to `bins/agent/tests/reconcile.rs`:

```rust
/// The gate is a per-NODE fact read off a cluster-scoped condition: node A's pass sets
/// `NamespaceReady=True` after creating the namespaces ITS workspaces need, and a workspace in a
/// team this node has never seen would sail past it into a 60 s `ensure_ssh` retry. The namespace
/// itself is what must answer.
#[tokio::test]
async fn the_namespace_gate_asks_about_this_workspace_s_own_namespace() {
    let tmp = tempfile::tempdir().unwrap();
    let binding = serde_json::json!({
        "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "OwnerBinding",
        "metadata": {"name": "r1-alice", "uid": "b-uid", "generation": 1},
        "spec": {"owner": "alice", "region": "r1", "nodeName": "node-a"},
        "status": {"observedGeneration": 1,
                   "conditions": [{"type": "NamespaceReady", "status": "True", "reason": "Converged",
                                   "message": "", "lastTransitionTime": "2000-01-01T00:00:00Z"}]},
    });
    let routes = vec![
        kloudlite_git_workspaces::kube_test::get("/apis/kloudlite-git.io/v1alpha1/ownerbindings/r1-alice", binding),
        kloudlite_git_workspaces::kube_test::not_found("/api/v1/namespaces/wt-alice-eng"),
    ];
    let (ctx, _rec) = ctx(tmp.path(), routes);

    assert!(!kloudlite_git_agent::binding::namespace_ready(&ctx, "r1", "alice", "eng").await.unwrap(),
            "a True condition from another node must not pass a namespace this node has not made");
}
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-git-agent-bin the_namespace_gate_asks_about` fails to compile (`this function takes 3 arguments but 4 were supplied`), and once the argument is added, on the assertion: the condition alone answers `true`.

- [ ] **Step 3: Implement** — replace `binding.rs:137-145`:

```rust
/// Whether this workspace's OWN namespace exists on this node's view of the cluster.
///
/// The `OwnerBinding` condition alone is not enough: `teams_in_use` is scoped to THIS node's
/// workspaces, so node A can report `NamespaceReady=True` for an owner whose team-B namespace no
/// node has made yet, and a workspace claimed on B would then fail into `ensure_ssh`'s 60 s retry
/// instead of waiting here. Both halves are asked — the condition says the binding pass has run at
/// all, the namespace says it ran for THIS team. A missing binding or namespace is "not ready",
/// never an error: it is the ordinary gap between a claim and the binding reconcile.
pub async fn namespace_ready(ctx: &Arc<Ctx>, region: &str, owner: &str, team: &str) -> Result<bool, ReconcileErr> {
    let Some(b) = get_binding(ctx, region, owner).await? else { return Ok(false) };
    if !b.status.is_some_and(|s| s.conditions.iter().any(|c| c.type_ == NAMESPACE_READY && c.status == "True")) {
        return Ok(false);
    }
    let ns = crd::ws_namespace(owner, team);
    Ok(Api::<k8s_openapi::api::core::v1::Namespace>::all(ctx.client.clone()).get_opt(&ns).await?.is_some())
}
```

  At the call site (ex-`controller.rs:1976`): `if !binding::namespace_ready(ctx, &w.spec.region, &w.spec.owner, &w.spec.team).await? {`.

- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-git-agent-bin && cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] **Step 5: Commit** — `git add bins/agent/src/binding.rs bins/agent/src/controller bins/agent/tests/reconcile.rs && git commit -m "Gate a workspace on its own namespace existing, not on a cluster-wide condition"`

---

### Task 18: Stop interpolating team slugs into a label selector

Detail finding 6, summary Low. `crates/workspaces/src/api.rs:1739`: `format!("{OWNER_LABEL} in ({})", owners.join(","))`, where `owners` comes from the directory's `teams_for`. A slug containing `,` or `)` widens or breaks the selector on a listing that decides `deleted:`. Not reachable today; every other selector in the file takes a single validated value.

**Files:** Modify `crates/workspaces/src/api.rs:1731-1743` (`live_parents`). Test in `crates/workspaces/tests/api_teams.rs`.

**Interfaces:** `live_parents(s: &ApiState, owner: &str, owners: &[String]) -> Option<BTreeMap<String, (String, String)>>` — unchanged signature; the selector is now built from validated segments only.

- [ ] **Step 1: Write the failing test** — append to `crates/workspaces/tests/api_teams.rs`:

```rust
/// A slug that is not a segment never reaches a label selector: `in (a,b)` is comma-delimited, so
/// one bad slug widens the set the listing decides `deleted:` from.
#[test]
fn the_owner_set_selector_drops_slugs_that_are_not_segments() {
    let owners = vec!["alice".to_string(), "bad,slug".to_string(), "ok-team".to_string(), "no)paren".to_string()];
    let sel = kloudlite_git_workspaces::api::owner_set_selector(&owners);
    assert_eq!(sel, format!("{}=in (alice,ok-team)", ""), "only validated segments");
}
```

  (Adjust the expected string to the exact `format!` the implementation below produces; the assertion that matters is that `bad,slug` and `no)paren` are absent.)

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-git-workspaces --test api_teams the_owner_set_selector` fails to compile: `error[E0425]: cannot find function 'owner_set_selector'`.

- [ ] **Step 3: Implement** — in `crates/workspaces/src/api.rs`, above `live_parents`:

```rust
/// `OWNER_LABEL in (…)`, built only from slugs that are single validated segments.
///
/// `in (a,b)` is comma-delimited and paren-terminated, so one slug carrying `,` or `)` widens or
/// breaks the set — on a listing that decides whether a row says "source deleted". Slugs are
/// directory-validated today; every other selector in this file takes a single validated value,
/// and this one now does too.
pub fn owner_set_selector(owners: &[String]) -> String {
    let safe: Vec<&str> =
        owners.iter().filter(|o| kloudlite_git_storage::store::valid_owner(o)).map(String::as_str).collect();
    format!("{OWNER_LABEL} in ({})", safe.join(","))
}
```

  and at `:1739`: `let lp = ListParams::default().labels(&owner_set_selector(owners));`

- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-git-workspaces && cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] **Step 5: Commit** — `git add crates/workspaces/src/api.rs crates/workspaces/tests/api_teams.rs && git commit -m "Build the owner-set label selector from validated slugs only"`

---

### Task 19: Test the NetworkPolicy peer structure

Test gap 1 — the review's "single most valuable untested invariant in the crate". `attach_egress` (`k8s.rs:1291`), `attach_ingress` (`k8s.rs:1312`), `allow_gateway_ingress` (`k8s.rs:1259`) and `allow_internet_egress`'s `CLUSTER_INTERNALS` (`k8s.rs:1223`). Each function's own comment says the two-peer form would let "any pod in the cluster … reach every sshd" and admit "another owner's workspace that happens to share the same id" — nothing asserts it.

**Files:** Test only — `crates/workspaces/src/k8s.rs`'s `mod tests` (append).

**Interfaces:** Consumes the four builders unchanged. No production code changes in this task.

- [ ] **Step 1: Write the failing test** — append to `crates/workspaces/src/k8s.rs`'s `mod tests`:

```rust
    /// ONE peer, always. `namespaceSelector` and `podSelector` in one element of `from`/`to` is an
    /// AND; split across two elements it is an OR, and every sshd in the cluster becomes reachable
    /// by any pod that labels itself correctly. The functions say so; this is what holds them to it.
    #[test]
    fn every_grant_ands_its_namespace_and_pod_selectors_in_one_peer() {
        let r = owner_ref();
        let cases: Vec<(&str, NetworkPolicy, &str)> = vec![
            ("attach_ingress", attach_ingress("env-1", "ws-alice", "ws-1", "alice", &r), "ingress"),
            ("allow_gateway_ingress", allow_gateway_ingress("ws-alice", "alice", &r), "ingress"),
            ("attach_egress", attach_egress("ws-alice", "ws-1", "env-1", "alice", &r), "egress"),
        ];
        for (name, pol, dir) in cases {
            let spec = serde_json::to_value(&pol).unwrap()["spec"].clone();
            let rules = spec[dir].as_array().unwrap_or_else(|| panic!("{name}: no {dir}"));
            assert_eq!(rules.len(), 1, "{name}: one rule");
            let peers = rules[0][if dir == "ingress" { "from" } else { "to" }].as_array().unwrap();
            assert_eq!(peers.len(), 1, "{name}: two peers is an OR, not an AND: {peers:?}");
            // And the selectors that must be there ARE there — a single peer with only a
            // namespaceSelector would pass the count above while opening the whole namespace.
            if name != "attach_egress" {
                assert!(peers[0].get("podSelector").is_some(), "{name}: no podSelector");
            }
            assert!(peers[0].get("namespaceSelector").is_some(), "{name}: no namespaceSelector");
        }
    }

    /// The gateway hole is port 22 and nothing else, from the gateway namespace and nothing else.
    #[test]
    fn the_gateway_hole_is_one_port_from_one_place() {
        let pol = allow_gateway_ingress("ws-alice", "alice", &owner_ref());
        let spec = serde_json::to_value(&pol).unwrap()["spec"].clone();
        let rule = &spec["ingress"][0];
        assert_eq!(rule["ports"], serde_json::json!([{"protocol": "TCP", "port": 22}]));
        assert_eq!(
            rule["from"][0]["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"],
            GATEWAY_NAMESPACE
        );
        assert_eq!(rule["from"][0]["podSelector"]["matchLabels"]["app"], "kloudlite-git-gateway");
    }

    /// `169.254.0.0/16` is the one that matters: on Azure `169.254.169.254` hands out the NODE's
    /// managed identity to anything that asks, which is a full escape from the cluster. RFC 1918
    /// covers pod, service and node networks without this code knowing their numbers.
    #[test]
    fn internet_egress_excludes_the_metadata_service_and_all_of_rfc_1918() {
        let pol = allow_internet_egress("ws-alice", "alice", &owner_ref());
        let spec = serde_json::to_value(&pol).unwrap()["spec"].clone();
        let block = &spec["egress"][0]["to"][0]["ipBlock"];
        assert_eq!(block["cidr"], "0.0.0.0/0");
        let except: Vec<String> =
            block["except"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
        for want in ["169.254.0.0/16", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"] {
            assert!(except.contains(&want.to_string()), "{want} is not excluded: {except:?}");
        }
        assert_eq!(spec["egress"].as_array().unwrap().len(), 1, "one rule; a second would union it open");
    }
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-git-workspaces every_grant_ands_its` — these pass at HEAD (they pin correct behaviour). Prove they bite: temporarily split `attach_ingress`'s single `from` peer into two elements and re-run; expected `attach_ingress: two peers is an OR, not an AND`. Revert before Step 3.
- [ ] **Step 3: Implement** — no production change; this task is the assertion. Confirm the deliberate break in Step 2 was reverted (`git diff crates/workspaces/src/k8s.rs` shows only the test additions).
- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-git-workspaces && cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] **Step 5: Commit** — `git add crates/workspaces/src/k8s.rs && git commit -m "Assert every network grant ANDs its selectors in one peer"`

---

### Task 20: Test the `write_resolv_conf` inode invariant

Test gap 3. `controller/workspace.rs`'s `write_resolv_conf` (ex-`controller.rs:1685`) documents "IN PLACE, never via a rename … do not 'fix' this into an atomic write" — the pod bind-mounts the file by inode. A test asserting the inode survives a rewrite is the only thing that will stop that refactor.

**Files:** Test only — `bins/agent/tests/reconcile.rs` (append). Requires `write_resolv_conf` to be reachable from the test crate: change its visibility to `pub` in `controller/workspace.rs` and re-export it from `controller/mod.rs`.

**Interfaces:** Consumes `pub fn write_resolv_conf(pool: &str, ws_id: &str, ws_ns: &str, env_ns: Option<&str>) -> Result<(), ReconcileErr>`.

- [ ] **Step 1: Write the failing test** — append to `bins/agent/tests/reconcile.rs`:

```rust
/// The pod bind-mounts this file BY INODE (`type: File`, no subPath), so a rewrite that replaces
/// the inode — `rename(2)`, the usual way to write a file atomically — leaves every running pod
/// reading the old one and attachment silently stops working. Verified on a live cluster; this
/// test is what stops someone "fixing" it into an atomic write.
#[test]
fn rewriting_a_resolv_conf_keeps_the_same_inode() {
    use std::os::unix::fs::MetadataExt;
    let tmp = tempfile::tempdir().unwrap();
    let pool = tmp.path().to_string_lossy().to_string();
    // The template the agent reads is its own `/etc/resolv.conf`; skip where that is unreadable.
    if std::fs::read_to_string("/etc/resolv.conf").is_err() {
        return;
    }

    kloudlite_git_agent::controller::write_resolv_conf(&pool, "ws-1", "ws-alice", None).unwrap();
    let path = kloudlite_git_workspaces::k8s::attach_file(&pool, "ws-1");
    let before = std::fs::metadata(&path).unwrap().ino();

    kloudlite_git_agent::controller::write_resolv_conf(&pool, "ws-1", "ws-alice", Some("env-abc")).unwrap();
    let after = std::fs::metadata(&path).unwrap();
    assert_eq!(before, after.ino(), "the file was replaced, not truncated: every running pod now reads the old inode");
    assert!(std::fs::read_to_string(&path).unwrap().contains("env-abc"), "and the new content did land");
}

/// A pre-migration pod mounted this path with a `subPath`, which kubernetes created as a
/// DIRECTORY. A node upgraded from that shape must clear it rather than leave the workspace with
/// no DNS for as long as the pod lives.
#[test]
fn a_directory_left_by_the_old_subpath_mount_is_replaced_by_the_file() {
    if std::fs::read_to_string("/etc/resolv.conf").is_err() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let pool = tmp.path().to_string_lossy().to_string();
    let path = kloudlite_git_workspaces::k8s::attach_file(&pool, "ws-1");
    std::fs::create_dir_all(&path).unwrap();

    kloudlite_git_agent::controller::write_resolv_conf(&pool, "ws-1", "ws-alice", None).unwrap();
    assert!(std::fs::metadata(&path).unwrap().is_file());
}
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-git-agent-bin rewriting_a_resolv_conf` fails with `error[E0603]: function 'write_resolv_conf' is private`.
- [ ] **Step 3: Implement** — in `bins/agent/src/controller/workspace.rs`, change `pub(crate) fn write_resolv_conf` to `pub fn write_resolv_conf` and add `pub use workspace::write_resolv_conf;` to `controller/mod.rs`'s re-export list, with the comment: `// pub so the inode invariant is assertable from the integration suite — see reconcile.rs.`
- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-git-agent-bin && cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] **Step 5: Commit** — `git add bins/agent/src/controller bins/agent/tests/reconcile.rs && git commit -m "Assert the attach resolv.conf is rewritten in place, never replaced"`

---

### Task 21: Test `mkdir_env_mounts` refuses a traversing folder

Test gap 4. `validate_mount` is tested in `model.rs`, but nothing asserts the CONTROLLER refuses a traversing folder before `create_dir_all` — which is where the escape actually lives (`controller/environment.rs`, ex-`controller.rs:3024`).

**Files:** Test only — a `#[cfg(test)] mod tests` at the foot of `bins/agent/src/controller/environment.rs` (`mkdir_env_mounts` is a private sync function; an in-crate test is the shape that reaches it, same as `janitor.rs`'s own `mod janitor_tests`).

**Interfaces:** Consumes `fn mkdir_env_mounts(live: &std::path::Path, services: &[model::Service]) -> Result<(), String>`.

- [ ] **Step 1: Write the failing test** — append to `bins/agent/src/controller/environment.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn svc(folder: &str) -> model::Service {
        serde_json::from_value(serde_json::json!({
            "name": "db", "image": "mongo:7", "command": [], "env": {},
            "ports": [], "mounts": [{"path": "/data/db", "folder": folder}],
        }))
        .unwrap()
    }

    /// `create_dir_all` on an unvalidated folder IS the escape — it would happily mkdir -p outside
    /// the subvolume before a pod ever bound it as a subPath. `validate_mount` is tested in
    /// `model.rs`; this asserts the controller actually calls it, which is where the escape lives.
    #[test]
    fn a_traversing_folder_makes_no_directory_and_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("live");
        std::fs::create_dir_all(&live).unwrap();
        for folder in ["../../etc", "..", "a/b", "/abs", ""] {
            assert!(mkdir_env_mounts(&live, &[svc(folder)]).is_err(), "accepted {folder:?}");
        }
        assert!(!tmp.path().join("etc").exists(), "nothing was created outside the subvolume");
        assert!(std::fs::read_dir(live.join("volumes")).map(|mut d| d.next().is_none()).unwrap_or(true));
    }

    /// The ordinary folder is made, once, under `volumes/`.
    #[test]
    fn a_valid_folder_is_created_under_volumes() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("live");
        std::fs::create_dir_all(&live).unwrap();
        mkdir_env_mounts(&live, &[svc("dbdata"), svc("dbdata")]).unwrap();
        assert!(live.join("volumes/dbdata").is_dir());
    }
}
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-git-agent-bin --lib a_traversing_folder_makes_no_directory` — passes at HEAD (`mkdir_env_mounts` already calls `validate_mount`). Prove it bites by commenting out the `model::validate_mount(m)?;` line: expected `accepted "../../etc"`. Restore it before Step 3.
- [ ] **Step 3: Implement** — no production change; confirm the deliberate break was reverted (`git diff bins/agent/src/controller/environment.rs` shows only the test module).
- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-git-agent-bin && cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] **Step 5: Commit** — `git add bins/agent/src/controller/environment.rs && git commit -m "Assert the controller refuses a traversing environment mount folder"`

---

### Task 22: Test `janitor::drop_stale_worktrees`' two guards

Test gap 5. `janitor.rs:287` deletes worktree subvolumes on any non-owner node; the empty-owner and `owner == me` guards (`:288-290`) are the whole safety argument and are unasserted anywhere.

**Files:** Test only — `bins/agent/src/janitor.rs`'s existing `mod janitor_tests` (append; it already has the `have_btrfs`/`Pool` imports and the `remove_dir_all` fallback for a Mac run).

**Interfaces:** Consumes `pub(crate) fn drop_stale_worktrees(engine: &Engine, volume: &str, owner: &str, me: &str) -> usize`.

- [ ] **Step 1: Write the failing test** — append to `mod janitor_tests` in `bins/agent/src/janitor.rs`:

```rust
    fn plant_worktree(pool: &std::path::Path, volume: &str, ws: &str) -> std::path::PathBuf {
        let p = pool.join("vol").join(volume).join("live").join(ws);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// An EMPTY owner is the window between release and takeover: the returning node may be about
    /// to take the volume back (`replicas: 1`), and its worktree is the only copy of that work.
    #[test]
    fn an_unowned_volume_keeps_every_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = Engine::new(Pool::new(tmp.path()));
        let wt = plant_worktree(tmp.path(), "v1", "ws-1");
        assert_eq!(drop_stale_worktrees(&engine, "v1", "", "node-a"), 0);
        assert!(wt.exists(), "an unowned volume's worktree is not stale");
    }

    /// I am the owner: these worktrees are mine and running.
    #[test]
    fn my_own_volume_keeps_every_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = Engine::new(Pool::new(tmp.path()));
        let wt = plant_worktree(tmp.path(), "v1", "ws-1");
        assert_eq!(drop_stale_worktrees(&engine, "v1", "node-a", "node-a"), 0);
        assert!(wt.exists(), "the owner never drops its own worktrees");
    }

    /// Owned elsewhere: what a takeover left behind, and the only case that deletes.
    #[test]
    fn a_volume_owned_elsewhere_drops_the_worktrees_left_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = Engine::new(Pool::new(tmp.path()));
        let wt = plant_worktree(tmp.path(), "v1", "ws-1");
        // A file beside the worktrees is not a worktree and must survive.
        std::fs::write(tmp.path().join("vol/v1/live/notes.txt"), b"x").unwrap();
        assert_eq!(drop_stale_worktrees(&engine, "v1", "node-b", "node-a"), 1);
        assert!(!wt.exists(), "the stale worktree goes");
        assert!(tmp.path().join("vol/v1/live/notes.txt").exists(), "a plain file is not a subvolume");
    }
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-git-agent-bin --lib an_unowned_volume_keeps_every_worktree` — passes at HEAD. Prove it bites by deleting the `owner.is_empty() ||` half of the guard at `janitor.rs:288`: expected `an unowned volume's worktree is not stale`. Restore before Step 3.
- [ ] **Step 3: Implement** — no production change; confirm the deliberate break was reverted.
- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-git-agent-bin && cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] **Step 5: Commit** — `git add bins/agent/src/janitor.rs && git commit -m "Assert stale worktrees are dropped only on a node that does not own the volume"`

---

### Task 23: Test `retain`'s transient arm

Test gap 6. `snapshot.rs:283-296` deletes "every other transient of this worktree"; nothing asserts it spares a non-transient commit or another worktree's sync point. This is a keep-bias path — the arm deliberately ignores `heads` and `spec.pinned` (the `ponytail:` note at `:291` says why), so its scoping is the entire safety argument.

**Files:** Test only — `bins/agent/src/snapshot.rs`'s test module (the one Task 9 added `test_ctx` to).

**Interfaces:** Consumes `async fn retain(ctx: &Arc<Ctx>, volume: &str, head: &str)`.

- [ ] **Step 1: Write the failing test** — append to `snapshot.rs`'s test module:

```rust
    fn snap(name: &str, volume: &str, worktree: &str, transient: bool, parent: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "Snapshot",
            "metadata": {"name": name, "uid": format!("{name}-uid")},
            "spec": {"volume": volume, "owner": "alice", "worktree": worktree,
                     "parent": parent, "pinned": false, "transient": transient},
            "status": {"phase": "ready"},
        })
    }

    /// One Ready transient per worktree, and NOTHING else: a commit is not a sync point, and
    /// another worktree's sync point belongs to that worktree. The arm ignores `heads` and
    /// `pinned` deliberately (see its ponytail note), so this scoping is its whole safety argument.
    #[tokio::test]
    async fn the_transient_arm_spares_commits_and_other_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let items = vec![
            snap("v1-newsync", "v1", "ws-1", true, "v1-oldsync"),
            snap("v1-oldsync", "v1", "ws-1", true, ""),
            snap("v1-commit", "v1", "ws-1", false, ""),
            snap("v1-otherws", "v1", "ws-2", true, ""),
        ];
        let routes = vec![Route {
            method: "GET",
            path: "/apis/kloudlite-git.io/v1alpha1/snapshots".into(),
            status: 200,
            body: list_of("Snapshot", items),
        }];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);

        retain(&ctx, "v1", "v1-newsync").await;

        let deleted: Vec<String> = rec.calls().into_iter().filter(|c| c.starts_with("DELETE")).collect();
        assert_eq!(deleted, vec!["DELETE /apis/kloudlite-git.io/v1alpha1/snapshots/v1-oldsync".to_string()], "{deleted:?}");
    }

    /// Keep-bias: a snapshot list this pass could not make deletes nothing at all.
    #[tokio::test]
    async fn a_snapshot_list_error_deletes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let routes = vec![Route {
            method: "GET",
            path: "/apis/kloudlite-git.io/v1alpha1/snapshots".into(),
            status: 500,
            body: serde_json::json!({}),
        }];
        let (ctx, rec) = test_ctx(tmp.path(), "node-a", routes);
        retain(&ctx, "v1", "v1-newsync").await;
        assert!(rec.calls().iter().all(|c| !c.starts_with("DELETE")), "{:?}", rec.calls());
    }
```

- [ ] **Step 2: Run it, expect failure** — `cargo test -p kloudlite-git-agent-bin --lib the_transient_arm_spares` — passes at HEAD. Prove it bites by removing `&& s.spec.worktree == worktree` from `snapshot.rs:293`: expected the `deleted` vector to gain `v1-otherws`.
- [ ] **Step 3: Implement** — no production change; confirm the deliberate break was reverted.
- [ ] **Step 4: Run tests and clippy** — `cargo test -p kloudlite-git-agent-bin && cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] **Step 5: Commit** — `git add bins/agent/src/snapshot.rs && git commit -m "Assert retention's sync-point arm spares commits and other worktrees"`

---

## Self-review

| Finding | Task |
|---|---|
| Detail 1 — `workspace_pod` does not re-validate `spec.name` (= summary High #5) | 1 |
| Detail 2 — `spec.owner` becomes a root-run path with no `valid_owner` (= summary Medium) | 1 |
| Detail 3 — `delete_env` cluster-wide list, swallowed error (= summary Medium) | 3 |
| Detail 4 — blocking syscalls on the reconcile reactor (= summary Medium) | 2 |
| Detail 5 — per-node `NamespaceReady` gate off a cluster-wide fact (= summary Low) | 17 |
| Detail 6 — team slugs interpolated into a label selector (= summary Low) | 18 |
| Detail 7 — `pull_beat` O(V) cluster-wide LISTs (= summary High #6) | 5, 6 |
| Detail 8 — a pod LIST per commit per source | 7 |
| Detail 9 — `flush_gate` lists every `VolumeReplica` per tick (= summary Medium `selectableFields`) | 4 |
| Detail 10 — retention lists every parent on every push (= summary Medium) | 9 |
| Detail 11 — already-marked janitor walks | deferred: no action asked for — the review's own verdict is "keep the `ponytail:` markers". Global Constraints require keeping them when editing nearby; no task touches `janitor.rs:42` or `:177`. |
| Detail 12 — split `controller.rs` (= summary Architecture #2) | 10, 11, 12, 13, 14, 15 (seven files: `stop`, `status`, `mod`+`run`, `volume`, `workspace`, `environment`) |
| Detail 13 — duplicated Workspace/Environment pairs: the four-times "parents on this node" query | 5, 6, 8, 9 |
| Detail 13 — the ~60-line worktree materialize block (`2013-2119` vs `2530-2574`) | deferred: the review itself says this "becomes obvious enough to factor" only AFTER the split, and factoring it is a behaviour-carrying merge of two subtly different arms (`clone_commit`, worktree name, `HeadUnknown`) — not a pure move, and not safe to plan blind against pre-split line numbers. Do it as a follow-up once Tasks 10-15 have landed and the two arms sit in two small files. |
| Detail 13 — `unclaim_kind`'s four per-kind closures, `claim_workspace`/`claim_environment`, api.rs's `restore`/`push`/`start`/`stop` pairs | deferred: pure duplication with no defect behind it, and `unclaim_kind` is on the keep-biased dead-node path where a serde-json rewrite risks more than the ~40 lines it saves. Same follow-up as above. |
| Detail 14 — `Done::lineage_tip` dead; phantom `WS_PEER_RECV_TIMEOUT_SECS` | 16 |
| Detail 14 — `model::Workspace::live_state` always Null | deferred: the review's own instruction is "retire it with the next web change" — removing it here breaks the web's parse, which is out of this plan's scope (summary §5 owns the web tier). |
| Detail 14 — one-implementation traits, config knobs | deferred: the review's verdict is "leave them" / "no unused knobs found". No action. |
| Test gap 1 — `attach_egress`/`attach_ingress`/`allow_gateway_ingress`/`CLUSTER_INTERNALS` | 19 |
| Test gap 2 — `workspace_pod` with a hostile `spec.name` | 1 |
| Test gap 3 — `write_resolv_conf` inode survives a rewrite | 20 |
| Test gap 4 — `mkdir_env_mounts` refuses a traversing folder | 21 |
| Test gap 5 — `janitor::drop_stale_worktrees` guards | 22 |
| Test gap 6 — `retain`'s transient arm | 23 |
| Test gap 7 — `teams_in_use`/`namespace_ready` interaction | 17 |
| Summary Architecture #1 — one shared listing threaded through the beats | 5, 6, 7, 8, 9 |
| Summary Architecture #3 — uniform untrusted-CR validation (`validate_spec`) | 1 |

**Task count: 23.**

Out of this plan's scope by assignment (owned by the deploy/manifests, server, registry and web passes of the same review): summary High #1, #2, #3, #4, #7, #8; every Medium under "Security and isolation" that lives in `deploy/`; the registry, server and web Mediums and Lows.
