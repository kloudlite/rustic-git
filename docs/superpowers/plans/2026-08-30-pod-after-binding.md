# Pod-After-Binding Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A workspace pod is created only once every claim it mounts is `Bound`, so the scheduler's
first attempt succeeds instead of hitting `FailedScheduling` and waiting out its backoff.

**Architecture:** One gate in `apply_workspace`, immediately before the pod is created, reading the
phase of the claims the pod mounts and requeuing on a short interval while any is unbound.

**Tech Stack:** Rust, kube-rs, Kubernetes PersistentVolumeClaims.

**Spec:** `docs/superpowers/specs/2026-08-30-pod-after-binding-design.md` — read it first. It carries
the two measurements this rests on (a pre-bound PVC binds in ~12 s; the scheduler then adds ~15 s of
backoff) and why the `volume_name` pre-binding that causes this cannot be removed.

## Global Constraints

- The gate guards CREATION only. A pod that already exists is never disturbed by a claim briefly
  reporting unbound — a running workspace must not be affected by this change at all.
- A claim whose status cannot be read is treated as NOT bound and requeued. A transient API error
  must never let the pod race ahead of its storage.
- The requeue interval is its own constant, deliberately shorter than `TICK` (15 s): reusing `TICK`
  would swap a 15 s scheduler backoff for a 15 s reconcile delay and save nothing.
- `ensure_storage` already runs before `ensure_profile` and both run before the pod; do not reorder
  anything. This adds a gate, it does not move existing work.
- Comments explain WHY at the density of `bins/server/src/router/route.rs`.
- Commit subject imperative sentence case, no tool attribution.
- Prefix cargo runs with `CARGO_INCREMENTAL=0` (disk pressure on this host), run them in the
  FOREGROUND with a long timeout, never wait on background monitors.
- `CARGO_INCREMENTAL=0 cargo test --workspace --locked` and
  `CARGO_INCREMENTAL=0 cargo clippy --workspace --all-targets --locked -- -D warnings` green.
  `tests/routing.rs` has a known pre-existing flake under parallel load
  (`a_real_git_push_and_clone_work_through_a_forwarding_node` and its ssh twin) — re-run it alone to
  confirm rather than chasing it.

---

### Task 1: Gate pod creation on the claims being bound

**Files:**
- Modify: `bins/agent/src/controller.rs` — a new helper beside the other pod predicates, a constant
  beside `TICK`, and the gate in `apply_workspace` immediately before the
  `match w.spec.desired_state { DesiredState::Running => {` block that builds and creates the pod
  (around `:2169`)
- Test: `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Consumes: `k8s::claim_name(id)` (the `live` claim), `k8s::nix_claim_name(id)`, `k8s::HOME_CLAIM`,
  `k8s::ATTACH_CLAIM`; the existing `Action::requeue`, `crd::condition`, `ws_conditions`,
  `wait_status`.
- Produces: `controller::claims_bound(pods_ns, names, ctx) -> Result<Option<String>, ReconcileErr>`
  — `None` when all are bound, `Some(name)` naming the first that is not. No other new public name.

- [ ] **Step 1: Write the failing tests**

Add to `bins/agent/tests/reconcile.rs`, following the file's existing fixture style:

```rust
/// The pod must not be created before its claims bind: the scheduler refuses an unbound claim and
/// then waits out its own backoff, which is the whole cost of creating a workspace.
#[tokio::test]
async fn a_workspace_whose_claims_are_pending_gets_no_pod() {
    let (ctx, rec) = ctx_with_pending_claims();
    let w: crd::Workspace = serde_json::from_value(ws_json_ready()).unwrap();
    let action = kloudlite_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();

    assert!(rec.calls().iter().all(|c| !(c.0 == "POST" && c.1.contains("/pods"))), "no pod is created");
    assert!(action != kube::runtime::controller::Action::await_change(), "it requeues rather than waiting for an event");
    let st = rec.sent("PATCH", WS_STATUS);
    let last = st.last().expect("a status write");
    let conds = last["status"]["conditions"].as_array().unwrap();
    assert!(
        conds.iter().any(|c| c["status"] == "True" && c["message"].as_str().unwrap_or("").contains("live-")),
        "the condition names the claim being waited on: {conds:?}"
    );
}

/// Once they bind, the pod is created as before.
#[tokio::test]
async fn a_workspace_whose_claims_are_bound_gets_its_pod() {
    let (ctx, rec) = ctx_with_bound_claims();
    let w: crd::Workspace = serde_json::from_value(ws_json_ready()).unwrap();
    kloudlite_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert!(rec.calls().iter().any(|c| c.0 == "POST" && c.1.contains("/pods")), "the pod is created");
}

/// The gate guards creation, not existence: a running workspace must not be disturbed by a claim
/// that momentarily reports unbound.
#[tokio::test]
async fn an_existing_pod_is_not_disturbed_by_an_unbound_claim() {
    let (ctx, rec) = ctx_with_pending_claims_and_a_running_pod();
    let w: crd::Workspace = serde_json::from_value(ws_json_ready()).unwrap();
    kloudlite_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert!(rec.calls().iter().all(|c| c.0 != "DELETE" || !c.1.contains("/pods")), "the pod is left alone");
}

/// A claim whose status cannot be read is not evidence that it is bound.
#[tokio::test]
async fn a_claim_that_cannot_be_read_is_treated_as_unbound() {
    let (ctx, rec) = ctx_with_claim_read_failing();
    let w: crd::Workspace = serde_json::from_value(ws_json_ready()).unwrap();
    let _ = kloudlite_git_agent::controller::apply_workspace(&w, &ctx).await;
    assert!(rec.calls().iter().all(|c| !(c.0 == "POST" && c.1.contains("/pods"))), "no pod on an unreadable claim");
}
```

The four `ctx_*` fixtures are named as this plan needs them; the file's real helpers differ. Build
them from the existing context builder by registering PVC routes that answer with the phase each
test needs (`Pending`, `Bound`, or an error). Follow how the file already registers routes — do not
invent a new fixture style, and do not add a second fake.

- [ ] **Step 2: Run them and watch them fail**

Run: `CARGO_INCREMENTAL=0 cargo test -p kloudlite-git-agent-bin claims`
Expected: FAIL — the pod is created regardless of claim phase.

- [ ] **Step 3: Add the constant**

Beside `TICK` in `bins/agent/src/controller.rs`:

```rust
/// How often to re-check a claim that has not bound yet.
///
/// Much shorter than `TICK` on purpose: a claim binds within seconds (the PV controller's resync,
/// ~12 s measured on this cluster), and requeuing on `TICK` would replace the scheduler's ~15 s
/// backoff with a 15 s reconcile delay — the same wait, moved. A handful of no-op reconciles per
/// workspace creation is the cheaper trade.
const BIND_POLL: Duration = Duration::from_secs(2);
```

- [ ] **Step 4: Add the predicate**

```rust
/// The first claim the pod would mount that is not `Bound`, or `None` when every one of them is.
///
/// A claim we cannot READ counts as unbound: a transient API error is not evidence that storage is
/// ready, and creating the pod on that assumption is what puts it in the scheduler's backoff queue.
async fn claims_bound(
    claims: &Api<PersistentVolumeClaim>,
    names: &[String],
    _ctx: &Arc<Ctx>,
) -> Result<Option<String>, ReconcileErr> {
    for name in names {
        match claims.get_opt(name).await {
            Ok(Some(c)) if c.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Bound") => {}
            Ok(_) => return Ok(Some(name.clone())),
            Err(e) => {
                tracing::warn!(claim = %name, error = %e, "reading a claim; treating it as unbound");
                return Ok(Some(name.clone()));
            }
        }
    }
    Ok(None)
}
```

- [ ] **Step 5: Add the gate**

In `apply_workspace`, immediately before the `let (phase, pod_ref) = match w.spec.desired_state {`
block, and only when the desired state is `Running` and the pod does not already exist:

```rust
    // The scheduler refuses a pod whose claims are unbound and then waits out its own backoff, so
    // creating the pod early costs ~15 s that the bind itself does not. The claims are already
    // created above; this only stops the pod racing them. Guards CREATION only — a running
    // workspace is never held up by a claim that momentarily reports unbound.
    if w.spec.desired_state == DesiredState::Running && pods.get_opt(&id).await?.is_none() {
        let names = vec![
            k8s::claim_name(&id),
            k8s::nix_claim_name(&id),
            k8s::HOME_CLAIM.to_string(),
            k8s::ATTACH_CLAIM.to_string(),
        ];
        let claims: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), &ns);
        if let Some(waiting) = claims_bound(&claims, &names, ctx).await? {
            let cond = crd::condition(
                "Progressing",
                true,
                "WaitingForStorage",
                &format!("waiting for the claim {waiting} to bind"),
                gen,
            );
            wait_status(w, prev, cond, ctx).await?;
            return Ok(Action::requeue(BIND_POLL));
        }
    }
```

Match the real signature of `wait_status` and of the surrounding `prev` handling — the snippet
above shows the shape, not necessarily the exact borrow. Add the `PersistentVolumeClaim` import.

- [ ] **Step 6: Run them and watch them pass**

Run: `CARGO_INCREMENTAL=0 cargo test -p kloudlite-git-agent-bin`
Expected: PASS, including the four new tests. Existing reconcile tests that create a pod may now
need their fixtures to report claims as `Bound`; update the FIXTURE, never an assertion. If any
existing test's assertion has to change to accommodate the gate, stop and report it — that would
mean the gate changed behaviour beyond ordering.

- [ ] **Step 7: Full suite and clippy**

Run: `CARGO_INCREMENTAL=0 cargo test --workspace --locked`
Then: `CARGO_INCREMENTAL=0 cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: both green.

- [ ] **Step 8: Commit**

```bash
git add bins/agent/src/controller.rs bins/agent/tests/reconcile.rs
git commit -m "Create a workspace pod only once its claims are bound"
```

---

## Self-review

**Spec coverage.** The gate, its short requeue and the unreadable-claim rule are Task 1 in full. The
spec's failure table: a claim that never binds (the workspace now stays `Progressing` naming it,
rather than a `Pending` pod — covered by test 1), a claim deleted after the gate (unchanged
behaviour, no test needed), `home`/`attach` behind (same gate, same requeue), an existing pod (test
3), an unreadable claim (test 4).

**Not covered on purpose.** The spec's final check — a clone reaching `Ready` with no
`FailedScheduling` event — is a post-deploy measurement on the cluster, not a task here.

**Type consistency.** `claims_bound` returns `Option<String>` naming the first unbound claim, used
once in the gate. `BIND_POLL` is used once. The four claim names come from the existing helpers and
match what `workspace_pod` mounts.

**Known soft spot.** The four `ctx_*` fixture names are as this plan needs them; the implementer is
told to build them from the file's real context builder. If registering a PVC route that returns an
error proves awkward in that harness, test 4 may need a different shape — the requirement is that
an unreadable claim does not create a pod, not that it be tested one particular way.
