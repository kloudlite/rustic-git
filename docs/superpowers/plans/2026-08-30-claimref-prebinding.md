# ClaimRef Pre-Binding Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A workspace's claim binds in under a second instead of anywhere from 0.4 s to 12.3 s, by
pre-binding from the PV side (`claimRef`) instead of the PVC side (`volumeName`).

**Architecture:** `local_pv` names the claim it is for; `claim` stops naming its volume; the PV is
created when absent rather than re-applied, so an already-bound PV's `claimRef` is never overwritten.

**Tech Stack:** Rust, kube-rs, Kubernetes static local PersistentVolumes.

**Spec:** `docs/superpowers/specs/2026-08-30-claimref-prebinding-design.md` — read it first. It
carries the measurements (`volumeName`: 0.4–12.3 s and unpredictable; `claimRef`: 561 ms with no
consumer, 1.3–2.1 s to a running pod) and the reason the pre-binding cannot simply be dropped.

## Global Constraints

- **Exclusivity is the point.** Every `nix-*` PV points at the same `/nix` and the `live-*` PVs
  differ only by path, so each volume must be pinned to exactly one claim. `claimRef` provides that
  from the PV side; nothing may weaken it.
- **`claimRef` carries NO uid.** A claim deleted and recreated gets a new uid; matching by
  namespace and name is what lets the new one bind.
- **The PV is created when absent, never re-applied.** An already-bound PV's `claimRef` was filled
  in by the binder with a uid and resourceVersion; applying ours over it would strip those. This is
  also honest about what a PV is — `nodeAffinity` is immutable, so an apply could never change the
  field that matters.
- The pod-after-binding gate, `BIND_POLL`, and the agent's `get` on `persistentvolumeclaims` all
  STAY. A `claimRef` claim binds without a consumer (561 ms measured), so the gate does not deadlock
  — it releases in under a second and keeps the `FailedScheduling` backoff from returning.
- Comments explain WHY at the density of `bins/server/src/router/route.rs`.
- Commit subject imperative sentence case, no tool attribution.
- Prefix cargo runs with `CARGO_INCREMENTAL=0` (disk pressure on this host), run them in the
  FOREGROUND with a long timeout, never wait on background monitors or background test runs.
- `CARGO_INCREMENTAL=0 cargo test --workspace --locked` and
  `CARGO_INCREMENTAL=0 cargo clippy --workspace --all-targets --locked -- -D warnings` green.
  `tests/routing.rs` has a known pre-existing flake under parallel load — re-run it alone.

---

### Task 1: Pre-bind from the PV, and stop re-applying it

**Files:**
- Modify: `crates/workspaces/src/k8s.rs` — `local_pv` (`:597`) gains the claim it is for; `claim`
  (`:642`) drops `volume_name`; the doc comment above `claim` explaining `volume_name` is replaced
  by one explaining `claimRef`
- Modify: `bins/agent/src/controller.rs` — `ensure_storage` (`:1689`) passes the claim name to
  `local_pv` and creates the PV when absent instead of applying it
- Test: the `mod tests` in `crates/workspaces/src/k8s.rs`, and `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Consumes: `k8s::PodContext`, the existing `ensure` and `create_if_absent` helpers in
  `bins/agent/src/controller.rs`.
- Produces: `k8s::local_pv(name, ns, claim, host_path, access_mode, capacity_gb, owner, ctx)` — two
  new parameters, the namespace and claim name it is bound to. `k8s::claim`'s signature is
  unchanged; only its body loses `volume_name`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/workspaces/src/k8s.rs`:

```rust
    /// Pre-binding moved to the PV: a claim naming its volume opts out of WaitForFirstConsumer,
    /// which is what made binding take anywhere from 0.4 s to 12.3 s.
    #[test]
    fn the_volume_names_its_claim_and_the_claim_names_no_volume() {
        let pv = local_pv("live-ws-1", "ws-acme", "live-ws-1", "/pool/vol/ws-1/live", "ReadWriteOnce", 20, "acme", &ctx());
        let cr = pv.spec.unwrap().claim_ref.expect("the PV names its claim");
        assert_eq!(cr.namespace.as_deref(), Some("ws-acme"));
        assert_eq!(cr.name.as_deref(), Some("live-ws-1"));
        assert!(cr.uid.is_none(), "no uid: a recreated claim must still match by name");

        let c = claim("ws-acme", "live-ws-1", "live-ws-1", "ReadWriteOnce", 20, "acme", &owner_ref());
        assert!(c.spec.unwrap().volume_name.is_none(), "the claim must not name its volume");
    }
```

And in `bins/agent/tests/reconcile.rs`:

```rust
/// The PV is created when absent and never re-applied: a bound PV's `claimRef` carries a uid the
/// binder filled in, and applying ours over it would strip it.
#[tokio::test]
async fn an_existing_persistent_volume_is_not_re_applied() {
    let (ctx, rec) = ctx_with_existing_pvs();
    let w: crd::Workspace = serde_json::from_value(ws_json_ready()).unwrap();
    kloudlite_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert!(
        rec.calls().iter().all(|c| !(c.0 == "PATCH" && c.1.contains("/persistentvolumes/"))),
        "an existing PV is left alone"
    );
}
```

`ctx_with_existing_pvs` is named as this plan needs it; build it from the file's real context
builder by registering PV routes that answer `200` for a GET. Match the tree.

- [ ] **Step 2: Run them and watch them fail**

Run: `CARGO_INCREMENTAL=0 cargo test -p kloudlite-git-workspaces names_its_claim` and
`CARGO_INCREMENTAL=0 cargo test -p kloudlite-git-agent-bin not_re_applied`
Expected: FAIL — `local_pv` takes six arguments and the claim still sets `volume_name`.

- [ ] **Step 3: Name the claim on the PV**

In `crates/workspaces/src/k8s.rs`, add the two parameters and the field:

```rust
pub fn local_pv(
    name: &str,
    ns: &str,
    claim: &str,
    host_path: &str,
    access_mode: &str,
    capacity_gb: u64,
    owner: &str,
    ctx: &PodContext,
) -> PersistentVolume {
```

and inside `PersistentVolumeSpec`, beside `local`:

```rust
            // Pre-bound from THIS side, not by `volumeName` on the claim. Every `nix-*` volume
            // points at the same `/nix` and the `live-*` ones differ only by path, so each must be
            // pinned to exactly one claim — but a claim that names its volume opts out of
            // `WaitForFirstConsumer`, and binding then waits on the PV controller's own schedule
            // (measured: 0.4 s to 12.3 s for the same shape). A `claimRef` pins it just as
            // exactly and binds in well under a second.
            //
            // No uid: a claim deleted and recreated gets a new one, and matching by namespace and
            // name is what lets the replacement bind.
            claim_ref: Some(ObjectReference {
                namespace: Some(ns.to_string()),
                name: Some(claim.to_string()),
                ..Default::default()
            }),
```

Add the `ObjectReference` import.

- [ ] **Step 4: Stop the claim naming its volume**

Delete `volume_name: Some(pv.to_string()),` from `claim`'s `PersistentVolumeClaimSpec`, and replace
the doc comment above `claim` that explains `volume_name` with one saying the pairing now comes from
the PV's `claimRef`. Keep the `pv` parameter — callers pass it and it stays part of the pair's
identity — or drop it and update `ensure_storage`; either is fine, but say which you chose and be
consistent.

- [ ] **Step 5: Create the PV when absent**

In `ensure_storage`, change the PV call from `ensure(...)` to `create_if_absent(...)`, passing the
claim's namespace and name to `local_pv`. Leave the claim on `ensure`. Add a comment:

```rust
    // Created, not applied: a bound PV's `claimRef` carries a uid and resourceVersion the binder
    // filled in, and a server-side apply of ours would strip them and put a live binding back in
    // front of the controller. `nodeAffinity` is immutable anyway, so an apply could never have
    // changed the field that matters.
```

- [ ] **Step 6: Run the tests**

Run: `CARGO_INCREMENTAL=0 cargo test -p kloudlite-git-workspaces -p kloudlite-git-agent-bin`
Expected: PASS. Existing tests that assert the PV or claim shape will need updating — update what
they EXPECT of the new shape, never what they are checking. If an assertion about exclusivity or
about the gate has to change, stop and report it.

- [ ] **Step 7: Full suite and clippy**

Run: `CARGO_INCREMENTAL=0 cargo test --workspace --locked`
Then: `CARGO_INCREMENTAL=0 cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 8: Commit**

```bash
git add crates/workspaces/src/k8s.rs bins/agent/src/controller.rs bins/agent/tests/reconcile.rs
git commit -m "Pre-bind a local volume from the PV rather than the claim"
```

---

## Self-review

**Spec coverage.** `claimRef` without uid (Step 3, asserted in Step 1), `volumeName` removed
(Step 4), create-only PV (Step 5, asserted in Step 1's second test). The spec's "no migration"
property needs no code: existing pairs are bound and nothing re-applies them, which is exactly what
Step 5 guarantees.

**Not covered on purpose.** The post-deploy measurement (a clone in single-digit seconds, three
repeats) is a verification step for the deploy, not a task.

**Type consistency.** `local_pv` gains `ns` and `claim` as parameters 2 and 3; the only caller is
`ensure_storage`, which already has both. `claim`'s signature is untouched.

**Known soft spot.** `ctx_with_existing_pvs` is named as this plan needs it; the implementer is told
to build it from the file's real fixtures. The four `ensure_storage` call sites (live, nix, home,
attach) all flow through the one helper, so none of them changes.
