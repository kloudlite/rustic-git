# One Kind of Node Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the session/env node-role concept from the agent and the pod renderer; every node with the pool hosts every kind.

**Architecture:** One boolean (`has_pool`, from `kloudlite.io/pool=true`) replaces `Ctx.roles`; `placement` selects on the pool label; the capacity rule is one constant. No CRD, no RBAC, no manifest change beyond the README.

**Tech Stack:** Rust (kube), the agent's existing tests.

**Spec:** `docs/superpowers/specs/2026-09-09-one-kind-of-node-design.md`

## Global Constraints

- The only node label read after this is `kloudlite.io/pool`. `kloudlite.io/session` and `kloudlite.io/env` appear nowhere in code or tests.
- Both claim watches run on every node with the pool, unconditionally. A node without it runs neither and warns `node.labels.missing` with reason `no-pool-label`.
- Capacity: 100 % of the guarantee, one constant, comment says why.
- No behaviour change on the fleet: every node already carries `pool=true`, so placement and claims are identical before and after. The roll is verified by the fast suite and by every workspace and environment staying where it is.
- Commit subjects imperative sentence case, no tool attribution; the hook rejects the string "CLAUDE.md" — write "the project guide".

---

### Task 1: The agent stops reading roles

**Files:**
- Modify: `bins/agent/src/lib.rs` (`node_roles` → `node_has_pool`)
- Modify: `bins/agent/src/controller/mod.rs` (`Ctx.roles: Vec<String>` → `Ctx.has_pool: bool`, `Ctx::new`)
- Modify: `bins/agent/src/controller/run.rs` (both claim watches on `has_pool`; delete the `ponytail:` note about dual-role nodes)
- Modify: `bins/agent/src/claim.rs` (`admissible_pct` → `ADMISSIBLE_PCT`)
- Test: `bins/agent/src/claim.rs` tests, `bins/agent/tests/reconcile.rs`, wherever `Ctx::new` is called in tests

**Interfaces:**
- Produces: `pub has_pool: bool` on `Ctx`; `Ctx::new(.., has_pool: bool, ..)` in the position `roles` held; `pub(crate) const ADMISSIBLE_PCT: u64 = 100;`.

- [ ] **Step 1: Failing tests.** In `claim.rs`, replace the two `admissible_pct` tests with one:

```rust
#[test]
fn every_node_admits_up_to_the_guarantee_and_no_further() {
    // One kind of node: the sheet's 80 % "env packing" was for a kind that does not exist.
    assert_eq!(ADMISSIBLE_PCT, 100);
    let n = node(serde_json::json!({"kloudlite.io/pool": "true"}), "8", "33554432Ki");
    assert!(may_claim_capacity(&n, /* 8 vCPU requested */ 8_000, 0).is_ok()); // keep the existing helper's real name and shape
}
```
and change every `"kloudlite.io/session": "true"` in `claim.rs` and `reconcile.rs` fixtures to `"kloudlite.io/pool": "true"`. Run `cargo test -p kloudlite-agent-bin` → FAIL (no `ADMISSIBLE_PCT`, `Ctx::new` arity).

- [ ] **Step 2: Implement.** `node_has_pool` (same shape as `node_roles`, one label, `bool`); `Ctx.has_pool`; in `run.rs`, `let claim_ws = ctx.has_pool.then(|| …)` and `let claim_env = ctx.has_pool.then(|| …)`, delete the `ponytail:` paragraph above `claim_ws`; `admissible_pct(..)` callers use `ADMISSIBLE_PCT` and the function is deleted; the `node.roles` info log becomes `node.pool` with `has_pool`.
- [ ] **Step 3:** `cargo test -p kloudlite-agent-bin`; `cargo clippy --workspace --all-targets -- -D warnings` → PASS; `grep -rn "kloudlite.io/session\|kloudlite.io/env\b\|\.roles" bins/agent` → empty.
- [ ] **Step 4: Commit** `"Agent: every node with the pool claims every kind"`.

### Task 2: Placement selects on the pool

**Files:**
- Modify: `crates/workspaces/src/k8s.rs` (`placement`, its two callers, the comment)
- Test: `crates/workspaces/src/k8s.rs` tests

**Interfaces:**
- Produces: `fn placement(spec: &mut PodSpec, node: &str)`.

- [ ] **Step 1: Failing test:**

```rust
#[test]
fn a_pod_is_pinned_to_its_node_and_to_the_pool_and_nothing_else() {
    let mut spec = PodSpec::default();
    placement(&mut spec, "node-a");
    let sel = spec.node_selector.unwrap();
    assert_eq!(sel.len(), 2);
    assert_eq!(sel["kloudlite.io/pool"], "true");
    assert_eq!(sel["kubernetes.io/hostname"], "node-a");
    let tol = &spec.tolerations.unwrap()[0];
    assert_eq!(tol.key.as_deref(), Some("kloudlite.io/pool"));
    assert_eq!(spec.automount_service_account_token, Some(false));
}
```
Run → FAIL (arity).
- [ ] **Step 2: Implement.** Drop the `role` parameter; selector and toleration on `kloudlite.io/pool`; callers `placement(&mut pod_spec, ctx.node_name)`. Replace the comment with why the pool label is the selector (the data is on this node's pool). Fix any existing test that asserted `kloudlite.io/session` or `kloudlite.io/env` in a rendered pod.
- [ ] **Step 3:** `cargo test -p kloudlite-workspaces`; clippy → PASS; `grep -rn "kloudlite.io/session\|kloudlite.io/env\b" crates` → empty.
- [ ] **Step 4: Commit** `"Place a pod on its node's pool, not on a kind of node"`.

### Task 3: Docs

**Files:**
- Modify: `deploy/k3s/README.md` (the two label lines → one; a rollout note), `docs/capacity-model.md` (one line), `CLAUDE.md` (grep for `session=true` / `env=true` / "env node" / "session node"; fix each sentence found)

- [ ] **Step 1:** README: `kubectl label node <node> kloudlite.io/pool=true   # hosts workspaces and environments` replaces lines 137–138; a dated note under the rollout history: "2026-09-09: node kinds removed; `kloudlite.io/session` and `kloudlite.io/env` are no longer read — `kubectl label node <n> kloudlite.io/session- kloudlite.io/env-` on each region when convenient."
- [ ] **Step 2:** capacity model: one line at the top of the node section stating the fleet is one kind of node at 100 % of the guarantee.
- [ ] **Step 3:** the project guide: every sentence naming a session node or an env node, rewritten; `grep -n "session node\|env node\|kloudlite.io/session\|kloudlite.io/env" CLAUDE.md docs/capacity-model.md deploy/k3s/README.md` → only the history note.
- [ ] **Step 4: Commit** `"Docs: one kind of node"`.

### Task 4: Fleet (controller runs this)

- [ ] **Step 1:** Ship; pin; `agent-daemonset.yaml` on the region; `deploy/roll.sh`.
- [ ] **Step 2:** Before and after the agent roll: `kubectl get workspaces,environments -o custom-columns=NAME:.metadata.name,NODE:.status.nodeName` — identical.
- [ ] **Step 3:** Agent logs: `node.pool has_pool=true` on all three nodes; no `node.labels.missing`.
- [ ] **Step 4:** Suspend the crons; `run-job.sh fast`; read `ws.create.p95`-class ids by result; restore. Merge and push both remotes.

## Self-review

Spec §1 → Task 1; §2 → Task 1; §3 → Task 2; §4 → Task 1; §5 → Task 3; §6 → Tasks 1, 2. Names consistent: `has_pool`, `node_has_pool`, `ADMISSIBLE_PCT`, `placement(spec, node)`, `kloudlite.io/pool`. No placeholder steps; the one helper name left to the implementer (`may_claim_capacity`) is flagged in the test as "the existing helper's real name".
