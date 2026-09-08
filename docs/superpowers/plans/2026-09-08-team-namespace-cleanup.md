# Team Namespace Cleanup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `wt-` namespace nothing uses is deleted by the api's resync beat, so a deleted team stops leaving a namespace behind.

**Architecture:** One prune function beside `project_all` in the beat that already prunes `OwnerKeys`, one new RBAC verb, one probe id. No new tier, no hook, no label, no migration.

**Tech Stack:** Rust (kube), the SLO probe, k3s RBAC.

**Spec:** `docs/superpowers/specs/2026-09-08-team-namespace-cleanup-design.md`

## Global Constraints

- Keep-biased: a failed `Workspace` list or `Namespace` list prunes NOTHING and returns. Same rule `project_all` follows.
- Only namespaces whose name starts with `wt-` are ever deleted. `ws-` is never pruned, and neither is anything without the `kloudlite.io/kind=workspace` label.
- Three guards, all required before a delete: not in the keep set, no pods in it, and `creationTimestamp` older than `KEYS_RESYNC_SECS`.
- The keep set is computed with `crd::ws_namespace(owner, team)` — never by re-deriving the name any other way.
- Runs only in the api's `user` role, from `keys::run_beat`, which the admin role never starts.
- House style: comments say why; no new dependencies.

---

### Task 1: The prune

**Files:**
- Modify: `crates/workspaces/src/api/keys.rs` (add `prune_namespaces`, call it from `run_beat`)
- Test: same file's `mod tests`

**Interfaces:**
- Produces: `pub(crate) async fn prune_namespaces(s: &ApiState)`, and a pure helper
  `fn stale_namespaces(keep: &BTreeSet<String>, seen: &[(String, i64, bool)], now: i64, max_age: i64) -> Vec<String>`
  where each `seen` entry is `(name, age_seconds, has_pods)`.
- Consumes: `crd::ws_namespace`, `k8s::KIND_LABEL`, `KEYS_RESYNC_SECS`.

- [ ] **Step 1: Write the failing tests** for `stale_namespaces` (pure, no cluster):

```rust
#[test]
fn only_an_unused_old_empty_team_namespace_is_stale() {
    let keep = BTreeSet::from(["wt-bob-abc".to_string(), "ws-bob".to_string()]);
    let seen = vec![
        ("wt-bob-abc".to_string(), 9999, false), // a workspace resolves to it
        ("ws-bob".to_string(), 9999, false),     // personal, never pruned
        ("ws-carol".to_string(), 9999, false),   // personal and unused, still never pruned
        ("wt-bob-dead".to_string(), 9999, false),// stale: the only one
        ("wt-bob-young".to_string(), 10, false), // too new to judge
        ("wt-bob-busy".to_string(), 9999, true), // has pods
    ];
    assert_eq!(stale_namespaces(&keep, &seen, 0, 300), vec!["wt-bob-dead".to_string()]);
}
```

- [ ] **Step 2: Run it** — `cargo test -p kloudlite-workspaces stale_namespaces` → FAIL (function not defined).
- [ ] **Step 3: Implement** `stale_namespaces` (pure: filter on the `wt-` prefix, `!keep.contains`, `age >= max_age`, `!has_pods`) and `prune_namespaces` around it: list `Workspace` (return on error), build `keep` from `ws_namespace(owner, team)`, list `Namespace` with the `KIND_LABEL=workspace` selector (return on error), read each one's age and whether `Api::<Pod>::namespaced(...).list(&ListParams::default().limit(1))` is non-empty, then delete each stale one, logging `keys.namespace.pruned` per success and `keys.namespace.prune.failed` per failure without aborting the rest.
- [ ] **Step 4: Call it** from `run_beat` right after `project_all(&s).await;`.
- [ ] **Step 5: Run** `cargo test -p kloudlite-workspaces` and `cargo clippy --workspace --all-targets -- -D warnings` → PASS.
- [ ] **Step 6: Commit** `"Api: prune the team namespaces nothing uses"`.

### Task 2: RBAC

**Files:**
- Modify: `deploy/k3s/api-rbac.yaml`

- [ ] **Step 1:** the `kloudlite-api` ClusterRole's `namespaces` rule gains `delete` beside `list`, and its comment says what bounds it: only `wt-` namespaces, only when no workspace resolves to them, only from the resync beat. Also add `pods: list` if the role does not already carry it — the prune's second guard reads pods; check the file first and say so in the commit if it was already there.
- [ ] **Step 2: Commit** `"Let the api delete a team namespace nothing uses"`.

### Task 3: The probe and docs

**Files:**
- Modify: `bins/slo/src/stages/experience_teams.rs`, `crates/workspaces/src/slo/catalogue.rs`, `deploy/slo.md`, `web/apps/web/src/lib/fixtures/superadmin.ts`, `CLAUDE.md`

**Catalogue row** (`Suite::Hourly`, stage `"14 · Experience"`, feature `"Workspaces"`):

| id | sli | target |
| --- | --- | --- |
| `team.namespace.reaped` | The namespace of a team this run deleted is gone within two resync beats | `avail(99.9)` |

- [ ] **Step 1:** read `experience_teams.rs` to find where the run's team is created and torn down, and what it records in `c.state`. The step asserts: after the team is deleted, the namespace `crd::ws_namespace(probe_user, team_slug)` is gone from the cluster, polled up to `2 * KEYS_RESYNC_SECS`. Skip with a reason when there is no kubeconfig or the run never made a team.
- [ ] **Step 2:** add the id to the stage's id list and its dispatch, and to the stage's exactly-once test.
- [ ] **Step 3:** add the row to all three catalogues, byte-identical.
- [ ] **Step 4:** `CLAUDE.md`, one sentence in "Workspaces and environments" beside the `OwnerKeys` prune: the same beat deletes a `wt-` namespace no workspace resolves to, which is what stops a deleted team leaving one behind.
- [ ] **Step 5:** `cargo test -p kloudlite-workspaces slo`, `cargo test -p kloudlite-slo-bin`, `cargo clippy --workspace --all-targets -- -D warnings`, `cd web && bun run test` → PASS.
- [ ] **Step 6: Commit** `"Probe: a deleted team leaves no namespace behind"`.

## Self-review

Spec §1 → Task 1; §2 → Task 2; §3 → Task 3; §4's failure table → Task 1's keep-biased returns and per-namespace error handling. Names consistent: `prune_namespaces`, `stale_namespaces`, `ws_namespace`, `KEYS_RESYNC_SECS`, `KIND_LABEL`, `team.namespace.reaped`. The 101 existing orphans need no task: Task 1's rule covers them on the first beat after the roll.
