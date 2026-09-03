# Server tier and deploy review fixes — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close every finding in the 2026-09-03 server/deploy review (3 Important, 7 Minor, 2 Cleanup) and the over-engineering audit cuts that land in the same files, without widening any grant or changing production behaviour to suit a test.

**Architecture:** Four kinds of change, kept apart so a reviewer can reject one and keep its neighbour: (a) YAML fences and RBAC tables in `deploy/k3s/` — the header table IS the role, so table and rule always move in the same commit; (b) one test-harness fix in `tests/routing.rs` that gives the harness the renewal beat production already has in `bins/server/src/lanes.rs`; (c) deletions of dead code the review names; (d) mechanical audit cuts batched by file. No new abstractions, no new dependencies.

**Tech Stack:** Rust (axum, tokio, kube-rs, slatedb), Kubernetes ValidatingAdmissionPolicy (CEL), k3s RBAC YAML, bash (`deploy/pin.sh`).

**Spec:**
- `docs/superpowers/reviews/2026-09-03-details/server-deploy.md` (the findings, numbered 1–12, plus the RBAC verbs table)
- `docs/superpowers/reviews/2026-09-03-details/audit.md` (the over-engineering cuts)
- `docs/superpowers/reviews/2026-09-03-codebase-review.md` (the roll-up and the suggested order)
- `CLAUDE.md` (the load-bearing rules; read "Workspaces and environments" and "Deploying" before touching `deploy/k3s/`)

## Global Constraints

- **No tool attribution in commits.** Commit subjects are imperative sentence case. No "Generated with", no Co-Authored-By trailer naming a tool.
- **Comments say WHY, never what.** Match the density of `bins/server/src/router/route.rs`. Keep any `// ponytail:` marker you edit near, and keep its stated ceiling true.
- **RBAC header tables ARE the role.** `deploy/k3s/agent-rbac.yaml` and `deploy/k3s/api-rbac.yaml` document that "a verb that is not in the table is not in the rules". A rule change and its table row move together, in the same commit. Never one without the other.
- **No grant wider than a verb the code actually calls.** Before adding or keeping a verb, produce the call site. Before deleting one, produce the grep that shows there is none.
- **Rust gate, run unpiped so the exit code is real:**
  `cargo test --workspace -- --test-threads=1; echo exit=$?`
  (`--test-threads=1` because `tests/routing.rs` binds real ports and the lease is wall-clock.)
- **Clippy gate:** `cargo clippy --workspace --all-targets --locked -- -D warnings`
  CI gates on `--workspace -- -D warnings` (lib and bin targets only). `--all-targets` has pre-existing lints in test targets; the bar is **no NEW warning in a file you touched**. Note any pre-existing warning you did not introduce in the commit body, do not fix it in the same commit.
- **YAML gate where a cluster is reachable:** `KUBECONFIG=.local/k3s.yaml kubectl apply --dry-run=server -f <file>`. If the cluster is unreachable, say so explicitly in the task's report — a skipped dry run is not a passed one. `--dry-run=client` is NOT a substitute: it does not compile the CEL.
- **Never widen `ownership::LEASE_TTL`** (`crates/storage/src/ownership/mod.rs:17`) and never re-claim before an assertion to make a test pass. The first changes production to suit a test; the second hides the lapse the test exists to catch.
- **Do not delete `bins/server/src/browse_api/volumes.rs`.** Its module doc rules it FROZEN; its retirement trigger is written in `deploy/k3s/README.md`. Review item 12 is "nothing to do" and is recorded here so it is not re-opened.

---

## File Structure

Files this plan creates or modifies, and what each is responsible for:

| File | Change | Responsibility |
|---|---|---|
| `deploy/k3s/agent-admission.yaml` | Modify (`:88-92` header, `:96-118` resourceRules, `validations`) | The DELETE fence and the file's stated ceiling |
| `deploy/k3s/agent-rbac.yaml` | Modify (header table ~`:80-115`, `deployments` rule ~`:243-249`) | Remove the dead grant; make the table true again |
| `deploy/k3s/api-rbac.yaml` | Modify (`:19-20` and its comment) | Drop `watch`/`update` neither used nor claimed |
| `deploy/k3s/workspace-admission.yaml` | Modify (comment above `:77`) | Say why `vol/` is deliberately not owner-scoped |
| `deploy/k3s/README.md` | Modify (node-join section) | The flannel `/32` step, numbered |
| `deploy/pin.sh` | Modify | Resolve each SHA to a digest and write `:<sha>@sha256:<digest>` |
| `deploy/*.yaml`, `deploy/k3s/*.yaml` | Modify (image pins) | Repin to HEAD with digests |
| `tests/routing.rs` | Modify (`node()` ~`:64-96`, tests at `:393`, `:1080`) | Renewal beat in the harness; fixture work before `claim()` |
| `crates/core/src/metrics.rs` | Modify (`:94`) | Drop the dead `/vol-agent/` class |
| `crates/workspaces/src/registry.rs` | Modify (`:69-77`, `:98-104`, `:122-127`, `:140-168`) | Delete four callerless `VolExt` methods and what only they use |
| `crates/api/src/lib.rs` | Modify (`:15-30`) | Delete the re-export shims |
| `crates/api/src/*.rs` | Modify (call sites) | Name the real paths |
| `crates/api/src/credentials.rs` | Modify (`:677`) | `rfc3339` via `chrono`, not `mongodb::bson` |
| `crates/workspaces/src/api.rs` | Modify (`:60-80`) | `Directory`'s methods become required |
| `crates/workspaces/tests/api_teams.rs`, `crates/workspaces/tests/api_user.rs` | Modify | Stubs say what they mean |
| `bins/gateway/src/lib.rs` | Delete | Four lines of re-export |
| `bins/gateway/src/main.rs`, `bins/gateway/Cargo.toml`, callers | Modify | Name `tunnel::` directly |
| `examples/views_bench.rs`, `examples/tree_bench.rs` | Delete | Wired to nothing; compiled on every `cargo test` |
| `crates/registry/src/blobs.rs`, `crates/storage/src/config.rs`, `bins/agent/src/nix.rs`, `crates/core/src/log.rs` | Modify (doc comments only) | Name each tunable's real setter |

---

### Task 1: Fence the agent's DELETEs to tenant namespaces

**Spec:** server-deploy.md Important 1. `services: delete` (added `4c7e94c9`) made the file's written residual false: a Service is not recreated by any kubelet, so deleting `kube-dns` in `kube-system` or `kubernetes` in `default` is a durable cluster-wide outage. Same for the pre-existing cluster-wide `networkpolicies: delete` and `statefulsets: delete`.

**Files:**
- Modify: `deploy/k3s/agent-admission.yaml` — the "Not covered, deliberately: DELETE" paragraph (~`:88-92`), `spec.matchConstraints.resourceRules` (~`:96-118`), `spec.validations`
- Test: `KUBECONFIG=.local/k3s.yaml kubectl apply --dry-run=server -f deploy/k3s/agent-admission.yaml`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: policy `rustic-git-agent-tenant-namespaces-only` now matches `DELETE` on `services`, `pods`, `statefulsets`, `networkpolicies`, with a SECOND validation entry keyed on `request.operation`. Task 2 edits the same directory but a different file.

- [ ] **Step 1: Capture the current server-side behaviour as the failing check**

The "test" here is the cluster's own CEL compiler plus a live delete. Record the before state:

```bash
KUBECONFIG=.local/k3s.yaml kubectl auth can-i delete services \
  --as=system:serviceaccount:kube-system:rustic-git-agent -n kube-system
```

Expected now: `yes` — and no admission policy stands in the way, which is the finding. If the cluster is unreachable, note it and continue; the dry-run in Step 4 is then also skipped and must be reported as skipped.

- [ ] **Step 2: Add the DELETE resourceRules**

In `deploy/k3s/agent-admission.yaml`, inside the `rustic-git-agent-tenant-namespaces-only` policy's `spec.matchConstraints.resourceRules`, append these four rules after the existing CREATE/UPDATE ones. Do NOT add `DELETE` to the existing rules' `operations` — the existing validation dereferences `object.kind`, which is null on a delete.

```yaml
      # DELETE is matched separately from CREATE/UPDATE: `object` is null on a delete and
      # `oldObject` carries the doomed object, so the two need different expression shapes and
      # must not share a rule. `namespaces` is absent on purpose — the agent holds no
      # `namespaces: delete`, so there is nothing to fence.
      - apiGroups: [""]
        apiVersions: ["v1"]
        operations: ["DELETE"]
        resources: ["services", "pods"]
      - apiGroups: ["apps"]
        apiVersions: ["v1"]
        operations: ["DELETE"]
        resources: ["statefulsets"]
      - apiGroups: ["networking.k8s.io"]
        apiVersions: ["v1"]
        operations: ["DELETE"]
        resources: ["networkpolicies"]
```

- [ ] **Step 3: Add the DELETE validation, ahead of the existing one**

In the same policy's `spec.validations`, insert this entry BEFORE the existing `object.kind == 'Namespace' ? ...` expression. Leave that existing expression exactly as it is — it now only ever sees CREATE/UPDATE, because the DELETE arm short-circuits first and the existing rules never matched DELETE.

```yaml
    # A delete the agent legitimately makes is always inside a namespace it reconciles: a
    # workspace pod, an environment's StatefulSets and Services, an attachment NetworkPolicy.
    # Deleting `kube-dns` or the `kubernetes` Service is a durable cluster-wide outage that no
    # kubelet repairs, which is why DELETE is fenced here and not left as an accepted residual.
    # `oldObject` IS populated on a delete (`object` is null), so this must be its own entry —
    # folding it into the expression below would dereference a null `object.kind`.
    - expression: >-
        request.operation != 'DELETE'
          || oldObject.metadata.namespace.startsWith('ws-')
          || oldObject.metadata.namespace.startsWith('wt-')
          || oldObject.metadata.namespace.startsWith('env-')
      message: "rustic-git-agent may only delete namespaced objects in ws-/wt-/env- namespaces"
```

And guard the existing expression so it is inert on a delete — one clause at its head, so the two entries are symmetric and neither depends on the resourceRules staying split:

```yaml
    - expression: >-
        request.operation == 'DELETE' ? true :
        object.kind == 'Namespace'
          ? (object.metadata.name.startsWith('ws-')
```

(the remainder of that expression is unchanged.)

- [ ] **Step 4: Rewrite the header's stated ceiling**

Replace the paragraph at ~`:88-92` that begins "Not covered, deliberately: DELETE." with:

```yaml
#    DELETE is covered too, by a SECOND validation entry keyed on `request.operation`: `object` is
#    null on a delete and `oldObject` holds the doomed object, so the two operations cannot share
#    an expression. The fence is the same one CREATE gets — the namespace must be `ws-`/`wt-`/
#    `env-`. It is not cosmetic: a Service is not recreated by any kubelet, so deleting `kube-dns`
#    in kube-system or `kubernetes` in default is a durable cluster-wide outage, and deleting the
#    tenant egress NetworkPolicies would take the isolation model with them.
#
#    Still not covered: cluster-scoped kinds the agent writes — `nodes: patch`, and its own
#    `namespaces: get,create,patch`. `nodes` is the broad grant already marked `ponytail:` in
#    agent-rbac.yaml, and namespace CREATE/UPDATE is fenced by the expression below.
```

- [ ] **Step 5: Server-side dry run**

Run: `KUBECONFIG=.local/k3s.yaml kubectl apply --dry-run=server -f deploy/k3s/agent-admission.yaml`
Expected: `validatingadmissionpolicy.admissionregistration.k8s.io/... configured (server dry run)` for both policies and both bindings, with no CEL compile error. A CEL error here surfaces as `spec.validations[0].expression: Invalid value` — read the message; it names the offending sub-expression.

If the cluster is unreachable: report "policy dry run SKIPPED — cluster unreachable" in the task report and do not claim it passed.

- [ ] **Step 6: Apply and verify the fence bites (only where the cluster is reachable)**

```bash
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/agent-admission.yaml
# A delete the agent must still be able to make: a pod in a tenant namespace (dry-run so
# nothing is actually removed).
KUBECONFIG=.local/k3s.yaml kubectl auth can-i delete pods \
  --as=system:serviceaccount:kube-system:rustic-git-agent -n ws-alice
```
Expected: `yes` (RBAC still allows it; the policy only refuses at admission time in a non-tenant namespace). The policy itself is proven by the dry run's CEL compile plus the expression's shape; a destructive live test against `kube-system` is not worth running.

- [ ] **Step 7: Commit**

```bash
git add deploy/k3s/agent-admission.yaml
git commit -m "Fence the agent's deletes to tenant namespaces"
```

Body: state that `oldObject` is what a DELETE carries, that the entry is separate because `object` is null, and that the header's residual paragraph was false since `services: delete` landed at `4c7e94c9`.

---

### Task 2: Delete the dead `apps/deployments` grant and repair the RBAC table

**Spec:** server-deploy.md Important 2 (dead grant) and Minor 8 (stale `git-token` Secret name in the same header table). The header table is documented as being the role, so a stale row is exactly the drift that file exists to prevent.

**Files:**
- Modify: `deploy/k3s/agent-rbac.yaml` — the `deployments (apps)` table row (`:102`), the `statefulsets (apps)` table rows (`:99-101`), the `deployments` rule (`:247-249`) and the sentence about it in the comment at `:243`, and the api RoleBinding's `git-token` mention
- Test: `KUBECONFIG=.local/k3s.yaml kubectl apply --dry-run=server -f deploy/k3s/agent-rbac.yaml`

**Interfaces:**
- Consumes: nothing from Task 1 (different file).
- Produces: ClusterRole `rustic-git-agent` no longer holds any `apps/deployments` verb.

- [ ] **Step 1: Prove the grant is dead**

```bash
grep -rn "Deployment" bins/ crates/ --include=*.rs | grep -v "^.*://"
```
Expected: only `bins/agent/src/controller/environment.rs:464`'s `async fn deployment_status(deployments: &Api<StatefulSet>, ...)` — a function NAME, whose type is `StatefulSet`. No `Api<Deployment>`, no `k8s_openapi::api::apps::v1::Deployment` import anywhere. Paste that output into the commit body; it is the whole evidence for the deletion.

- [ ] **Step 2: Delete the rule**

Remove from `deploy/k3s/agent-rbac.yaml`:

```yaml
  - apiGroups: ["apps"]
    resources: ["deployments"]
    verbs: ["get", "delete"]
```

and drop the trailing sentence from the comment above the `statefulsets` rule — it currently reads "… and deleted on stop. `deployments` only to find and delete the legacy ones a StatefulSet replaced." Keep everything up to "deleted on stop." and stop there.

- [ ] **Step 3: Repair the header table**

Delete this row entirely:

```
#   deployments (apps)                     get,delete                   legacy migration only
```

And correct the `statefulsets (apps)` rows so the table matches the rule (`get,list,watch,create,patch,delete`). The `deployment_status` reference in the table is a live function whose type is a StatefulSet, so the row keeps it but says so:

```
#   statefulsets (apps)                    get,list,watch               deployment_status (reads a
#                                                                       StatefulSet despite the
#                                                                       name); Controller watches
#                                          create,patch                 ensure; restore_gate scale
#                                          delete                       stop — fenced to tenant
#                                                                       namespaces by
#                                                                       agent-admission.yaml
```

- [ ] **Step 4: Fix the stale Secret name (Minor 8)**

In the same file, the api RoleBinding's comment says the RoleBinding exists "so the API can write that namespace's git-token Secret". There is no `git-token` Secret — `crates/workspaces/src/k8s.rs:177` defines `USER_KEY_SECRET = "user-key"` and `api-rbac.yaml` pins `resourceNames` to exactly that. Change `git-token` to `user-key`. Verify first:

```bash
grep -rn "git-token" deploy/ crates/ bins/ web/ ; grep -n "USER_KEY_SECRET" crates/workspaces/src/k8s.rs
```
Expected: no `git-token` anywhere except the line you are about to fix.

- [ ] **Step 5: Say that the DELETE verbs are now fenced**

Add one line to the `services` and `networkpolicies` table rows' notes, since Task 1 changed what their ceiling is:

```
#                                                                       delete is fenced to
#                                                                       ws-/wt-/env- namespaces by
#                                                                       agent-admission.yaml
```

- [ ] **Step 6: Dry run and apply**

Run: `KUBECONFIG=.local/k3s.yaml kubectl apply --dry-run=server -f deploy/k3s/agent-rbac.yaml`
Expected: `clusterrole.rbac.authorization.k8s.io/rustic-git-agent configured (server dry run)` with no error. Then apply for real; removing a verb the code never calls cannot break a running agent, and the next reconcile proves it (`kubectl -n kube-system logs ds/rustic-git-agent --tail=50` shows no `403` lines).

- [ ] **Step 7: Commit**

```bash
git add deploy/k3s/agent-rbac.yaml
git commit -m "Drop the dead deployments grant and repair the RBAC table"
```

- [ ] **Step 8: Hand off the rename (out of scope here)**

`bins/agent/src/controller/environment.rs:464` `deployment_status` should become `service_status_of` — the name is what hides the dead grant from a grep. That file is the agent's, not this review's scope. Note it in the commit body and in the agent review's follow-ups; do not rename it in this commit.

---

### Task 3: Give the routing test harness the renewal beat production has

**Spec:** server-deploy.md Important 3. `ownership::LEASE_TTL` is 10 s wall-clock (`crates/storage/src/ownership/mod.rs:17`); `bins/server/src/lanes.rs:36-49` renews on a beat, `tests/routing.rs`'s `node()` deliberately does one `election_tick()` and no loop. A claim taken at t=0 is dead at t=10 s, `App::route_for` filters expired entries (`crates/app/src/lib.rs:412`), and the forwarding node then takes the claim path — so `assert_eq!(b.store.pool.warm_count(), 0)` fails and A's `1` becomes `0`. On a loaded box, `git init` + a 3 MiB write + `git commit` + `git push` + `git clone` through three in-process axum servers easily exceeds 10 s.

**Files:**
- Modify: `tests/routing.rs` — `node()` (~`:64-96`, right after the existing `app.election_tick().await.unwrap()`), and the fixture ordering in `a_real_git_push_and_clone_work_through_a_forwarding_node` (`:393`) and `a_real_ssh_clone_works_through_a_forwarding_node` (`:1080`)
- Test: `tests/routing.rs` itself — this task's deliverable IS the test suite passing under an artificial delay

**Interfaces:**
- Consumes: `App::election_tick(&self) -> Result<()>` (`crates/app/src/lib.rs:232`), `App::renew_once(&self) -> Result<()>` (`:708`), `App::claim(&self, repo: &str) -> Result<Grant>` (`:593`). All already exist; nothing new is added to `App`.
- Produces: a `node()` whose returned `Node` holds a claim across an operation longer than `LEASE_TTL`.

- [ ] **Step 1: Write the failing test — reproduce the lapse deterministically**

Add this to `tests/routing.rs`. It is the whole finding in miniature: claim, wait past `LEASE_TTL`, and assert the forwarding node still forwards.

```rust
/// The harness must renew what it holds, or any test whose middle takes longer than
/// `LEASE_TTL` silently stops testing forwarding and starts testing claiming — the failure
/// mode that made the real git/ssh tests flake only under load.
#[tokio::test]
async fn a_claim_outlives_an_operation_longer_than_the_lease() {
    let e = common::env().await;
    let f = fleet(3);
    let repo = "alice/web".to_string();
    let (o, n) = repo.split_once('/').unwrap();
    e.store.create_repo(o, n).await.unwrap();
    let _leader = node(e.store.os.clone(), LEADER, &f).await;
    let a = node(e.store.os.clone(), "rustic-git-1", &f).await;
    let b = node(e.store.os.clone(), "rustic-git-2", &f).await;
    a.app.claim(&repo).await.unwrap();

    // Longer than LEASE_TTL (10 s), which is what a loaded box's git fixture work costs.
    tokio::time::sleep(std::time::Duration::from_secs(13)).await;

    // B must still forward, not claim: it opens nothing.
    let url = format!("http://{}/{repo}.git/info/refs?service=git-upload-pack", b.public);
    let _ = reqwest::get(&url).await.unwrap();
    assert_eq!(b.store.pool.warm_count(), 0, "B claimed the repo — A's lease lapsed");
    assert_eq!(a.store.pool.warm_count(), 1, "A no longer holds the repo it claimed");
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --test routing a_claim_outlives_an_operation_longer_than_the_lease -- --test-threads=1 --nocapture; echo exit=$?`
Expected: FAIL, `B claimed the repo — A's lease lapsed`, `left: 1, right: 0`. If it passes, you have not reproduced the finding — check that `LEASE_TTL` is still 10 s and that the sleep exceeds it.

- [ ] **Step 3: Add the renewal beat to `node()`**

In `tests/routing.rs`, immediately after the existing `app.election_tick().await.unwrap();` in `node()` (and before the release hook is set), add:

```rust
    // Production renews every held lease on a beat (`bins/server/src/lanes.rs`). Without it a
    // claim taken here is dead in LEASE_TTL (10 s) and any test whose middle runs longer —
    // a real git push, an ssh clone — silently stops testing forwarding and starts testing
    // claiming, which is the "only under load" flake. The 3 s cadence is LEASE_TTL/3, the same
    // ratio lanes.rs uses, so a single missed beat is survivable.
    let a5 = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let _ = a5.election_tick().await;
            let _ = a5.renew_once().await;
        }
    });
```

Also amend the existing comment above `election_tick` — it currently says "and no loop: renewal cadence is lanes.rs's, not what these tests prove", which is no longer true. Replace that clause with: "and then a renewal beat, because a lease that lapses mid-test changes what the test measures. A test that needs a failover still advances a follower's clock past LEADER_TTL and ticks it by hand."

- [ ] **Step 4: Run the new test — it should now pass**

Run: `cargo test --test routing a_claim_outlives_an_operation_longer_than_the_lease -- --test-threads=1 --nocapture; echo exit=$?`
Expected: PASS.

- [ ] **Step 5: Close the second half — the window before the first forwarded request**

`renew_once` renews `pool.warm_repos()`, i.e. repos this node has OPEN. Between `a.app.claim(&repo)` and A's first forwarded request the repo is claimed but not open, so the beat does not cover it. Move the fixture work to before the claim.

In `a_real_git_push_and_clone_work_through_a_forwarding_node` (`tests/routing.rs:393`), the current order is:

```rust
    a.app.claim(&repo).await.unwrap(); // A holds it; every request to B is forwarded
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    let url = format!("http://x:{token}@{}/{repo}.git", b.public);
    let git = |dir: &std::path::Path, args: &[&str]| { /* … */ };
```

Move every line that touches the filesystem — the tempdir, `work`, the `git` closure, `git init`, the 3 MiB write, `git add`, `git commit` — ABOVE the `claim()` call, so the claim is immediately followed by the network work that opens the repo. Only the `claim()` and the requests that follow it stay in place, with the comment kept and extended:

```rust
    // The fixture FIRST: `renew_once` only renews repos this node has OPEN, so the window
    // between the claim and A's first forwarded request is not covered by the beat. Doing the
    // slow local work before the claim keeps that window at roughly one HTTP round trip.
    a.app.claim(&repo).await.unwrap(); // A holds it; every request to B is forwarded
```

- [ ] **Step 6: Same move in the ssh test**

In `a_real_ssh_clone_works_through_a_forwarding_node` (`tests/routing.rs:1080`), the same shape: `a.app.claim(&repo)` is followed by `gen_host_key`, an ssh listener bind, a spawn, and a whole clone/commit/push seed cycle over A's public port. Move the ssh key generation, the host key, the listener bind and the seed clone/commit/push ABOVE the `claim()` call. The `tokio::spawn` of `rustic_git_git::ssh::serve` and everything after may stay. Carry the same comment as Step 5.

Note: the seed push targets `a.public`, which opens the repo on A anyway — after the move it opens A's copy before the claim, which is fine: `claim` on a repo this node already has open is a no-op re-assert, and the beat covers it from then on.

- [ ] **Step 7: Confirm the other two named tests already do this**

`a_push_after_a_stray_opener_succeeds` and `an_unhealthy_node_stops_serving_but_still_forwards` (`:663`) are named in the review as also affected but were found to already sequence the fixture before the claim. Verify by reading them; if either does not, apply the same move. Record which you checked in the commit body.

- [ ] **Step 8: Run the whole routing suite, repeatedly**

Run: `cargo test --test routing -- --test-threads=1; echo exit=$?`
Then, because one green run is noise for a load-sensitive flake, run it three more times under load:

```bash
for i in 1 2 3; do cargo test --test routing -- --test-threads=1; echo "run $i exit=$?"; done
```
Expected: `exit=0` on all four. Any `warm_count` assertion failure means a lease still lapsed — do NOT widen `LEASE_TTL`; find which operation exceeded the beat.

- [ ] **Step 9: Full gate**

Run: `cargo test --workspace -- --test-threads=1; echo exit=$?`
Expected: `exit=0`.

- [ ] **Step 10: Commit**

```bash
git add tests/routing.rs
git commit -m "Renew held leases in the routing test harness"
```

Body: name `LEASE_TTL` = 10 s, `lanes.rs:36-49` as the production beat this mirrors, and `renew_once`'s warm-repos scope as why the fixture order also had to move.

---

### Task 4: Delete the four callerless `VolExt` methods

**Spec:** server-deploy.md Cleanup 11. `move_ref`, `ref_commit`, `commit` and `region` have zero callers anywhere including tests, plus the constants and helpers only they use. Clippy cannot catch it: they are `pub` trait methods in a library crate. `/v1` writes `Snapshot` CRs now, so nothing writes the volume registry and the region stamp those methods defend has no reader.

**Files:**
- Modify: `crates/workspaces/src/registry.rs` — trait declarations at `:98-104` (`move_ref`, `ref_commit`, `commit`, `region`), impl bodies at `:140-168`, `REGION_KEY:69`, `REF_PREFIX:71`, `ref_key:75-77`, and the region-stamping branch inside `append_commits` (`:122-127`)
- Test: `cargo test --workspace` — nothing should reference them, so the compiler is the test

**Interfaces:**
- Consumes: nothing.
- Produces: `VolExt` with exactly four methods — `vol_db`, `vol_exists`, `append_commits`, `history`. Anything later referring to `VolExt::region` is referring to a method that no longer exists.

- [ ] **Step 1: Prove there are no callers**

```bash
grep -rn "move_ref\|ref_commit\|\.region(\|ref_key\|REGION_KEY\|REF_PREFIX" --include=*.rs crates bins tests | grep -v "^crates/workspaces/src/registry.rs"
```
Expected: empty. `VolExt::commit` needs its own grep because the name is common:

```bash
grep -rn "VolExt" --include=*.rs crates bins tests
grep -rn "\.commit(" --include=*.rs crates bins tests | grep -i vol
```
Expected: `VolExt` is imported only by `bins/server/src/browse_api/volumes.rs` (which calls `vol_exists` at `:107` and `history` at `:110`) and by `crates/workspaces/tests/*` for `append_commits`. Paste this into the commit body.

- [ ] **Step 2: Delete the four trait declarations**

From `crates/workspaces/src/registry.rs`, remove from the `pub trait VolExt` block:

```rust
    /// Moves `ref_name` to `commit`, refusing an unknown commit id (the caller answers 404/409;
    /// this just reports `false`).
    async fn move_ref(&self, owner: &str, name: &str, ref_name: &str, commit: &str) -> Result<bool>;
    async fn ref_commit(&self, owner: &str, name: &str, ref_name: &str) -> Result<Option<String>>;
    async fn commit(&self, owner: &str, name: &str, id: &str) -> Result<Option<CommitRecord>>;
```

and

```rust
    /// The region that owns this volume, or `None` if nothing has been written to it yet.
    async fn region(&self, owner: &str, name: &str) -> Result<Option<String>>;
```

Keep `vol_db`, `vol_exists`, `append_commits` and `history` with their doc comments verbatim — `vol_exists`'s "opening CREATES" comment is load-bearing.

- [ ] **Step 3: Delete the four impl bodies**

Remove `impl VolExt for Store`'s `region` (`:140-…`), `move_ref` (`:149-…`), `ref_commit` (`:158-…`) and `commit` (`:163-…`) blocks in full.

- [ ] **Step 4: Delete what only they used**

Remove the `REGION_KEY` const and its doc comment (`:65-69`), the `REF_PREFIX` const (`:71`), and the `ref_key` helper (`:75-77`). Then remove the region-stamping branch from `append_commits`:

```rust
        // Claim the volume for its region on the first record ever written, and never rewrite it:
        // the stamp is what later requests are checked against, so a writer that could overwrite it
        // could also hand the volume to itself.
        if let Some(first) = records.first() {
            if !first.region.is_empty() && db.get(REGION_KEY).await?.is_none() {
                db.put(REGION_KEY, first.region.as_bytes().to_vec()).await?;
            }
        }
```

`CommitRecord.region` itself stays — it is a serialized field of records the frozen read surface still returns; only the extra point-read stamp goes.

- [ ] **Step 5: Note why the trait survives**

The trait's doc comment at `:81-83` already explains it exists because of the orphan rule (`Store` is foreign to this crate). Leave it exactly as it is — the reason a four-method trait with one impl is not itself over-engineering is written there.

- [ ] **Step 6: Build and test**

Run: `cargo test --workspace -- --test-threads=1; echo exit=$?`
Expected: `exit=0`. A compile error naming one of the deleted methods means Step 1's grep missed a caller — restore that one method with its grep as the reason.

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: no new warning in `crates/workspaces/src/registry.rs`. A newly-unused import (e.g. of `CommitRecord`, if `region` was its only other use) is a real warning — remove it.

- [ ] **Step 7: Commit**

```bash
git add crates/workspaces/src/registry.rs
git commit -m "Delete the callerless VolExt methods and the region stamp"
```

Body: `/v1` writes `Snapshot` CRs, so nothing writes the volume registry; `browse_api/volumes.rs` stays FROZEN and still reads `vol_exists` and `history`. Also record review item 12: `browse_api/volumes.rs` is deliberately kept, its dead half was already removed at `3bcdf368`, and its retirement trigger lives in `deploy/k3s/README.md` — nothing to do there.

---

### Task 5: The minors — metrics class, api RBAC verbs, hostPath comment, node-join step

**Spec:** server-deploy.md Minors 6, 7, 9, 10. Four small, independent edits batched because none carries its own test cycle; Minors 4 and 5 (image pinning) are Task 6 because they share `deploy/pin.sh`.

**Files:**
- Modify: `crates/core/src/metrics.rs:94` (Minor 9)
- Modify: `deploy/k3s/api-rbac.yaml:19-20` and its comment (Minor 7)
- Modify: `deploy/k3s/workspace-admission.yaml` — the comment above the hostPath expression at `:70-83` (Minor 6)
- Modify: `deploy/k3s/README.md` — node-join checklist (Minor 10)

**Interfaces:**
- Consumes: nothing.
- Produces: `route_class` no longer returns `"vol-agent"`; ClusterRole `rustic-git-api` no longer holds `watch`/`update` on `workspaces`/`environments`.

- [ ] **Step 1 (Minor 9): Prove `/vol-agent/` is dead, then delete the branch**

```bash
grep -rn "vol-agent" --include=*.rs --include=*.yaml --include=*.ts . | grep -v "^./target"
```
Expected: only `crates/core/src/metrics.rs:94`. The agent-facing volume-registry routes were deleted at the workspaces cutover, so this is one dead `starts_with` per request that also implies a route surface that does not exist.

Delete from `route_class`:

```rust
    } else if path.starts_with("/vol-agent/") {
        "vol-agent"
```

- [ ] **Step 2 (Minor 9): Test**

Run: `cargo test -p rustic-git-core; echo exit=$?`
Expected: `exit=0`. If a unit test asserts the `"vol-agent"` class, delete that assertion too — it is testing a dead route.

- [ ] **Step 3 (Minor 7): Prove `watch` and `update` are unused, then drop them**

```bash
grep -n "\.watch(\|watcher::\|\.replace(\|\.replace_status(" crates/workspaces/src/api.rs
```
Expected: empty — every read is a one-shot `get`/`list`, every write a `create`/`patch`/`delete`. The file's own `snapshots` comment already says "No `watch`: nothing in the API watches", so the rule above it contradicts the rule below it.

In `deploy/k3s/api-rbac.yaml:20`, change:

```yaml
    verbs: ["get", "list", "watch", "create", "patch", "update", "delete"]
```
to:
```yaml
    verbs: ["get", "list", "create", "patch", "delete"]
```

and update the comment at `:23` so it reads the same way the `snapshots` comment does — no `watch` (nothing in the API watches; every read is a one-shot list on a request) and no `update` (every write is a server-side apply or a JSON patch, never a whole-object replace).

- [ ] **Step 4 (Minor 7): Dry run**

Run: `KUBECONFIG=.local/k3s.yaml kubectl apply --dry-run=server -f deploy/k3s/api-rbac.yaml`
Expected: `configured (server dry run)`. Then apply; removing an unused verb cannot break a running api pod, and `/v1` list/create/patch/delete are covered by `./tests/ws_e2e.sh` on a Linux box with the cluster (not this Mac — expect exit 77 here, which is a skip, not a pass).

- [ ] **Step 5 (Minor 6): Say why `vol/` is not owner-scoped**

`deploy/k3s/workspace-admission.yaml:77` admits anything under `/wspool-prod/vol/` while `:79-82` pin `homes`/`homecache` to the pod's `rustic-git.io/owner` label. Making `vol/` symmetric needs the owner in the pool path (`{pool}/vol/{owner}/{id}`) — a layout change across the agent, out of this review's scope. Until then, write the ceiling down. Add above the expression:

```yaml
    # `vol/` is deliberately NOT owner-scoped, unlike `homes`/`homecache` below: the volume pool
    # is laid out `{pool}/vol/{id}` with no owner segment, so there is nothing for the CEL to
    # compare against. The residual is a pod-builder bug mounting another tenant's subvolume —
    # not reachable from outside, because the path comes from the agent and volume ids are
    # opaque. ponytail: the fix is `{pool}/vol/{owner}/{id}`, at which point this line becomes
    # the same comparison the two below already make.
```

- [ ] **Step 6 (Minor 6): Dry run**

Run: `KUBECONFIG=.local/k3s.yaml kubectl apply --dry-run=server -f deploy/k3s/workspace-admission.yaml`
Expected: `configured (server dry run)`. A comment-only change cannot fail CEL compilation, but run it anyway — the file is one policy and a mis-indented comment inside a block scalar silently becomes part of the expression.

- [ ] **Step 7 (Minor 10): Number the flannel step in the node-join runbook**

`deploy/k3s/system-netpol.yaml` says loudly that its flannel `/32` list is HAND-MAINTAINED and that a new node's `flannel.1` `/32` must be added BEFORE its agent first mounts. The failure is survivable — `mount_homes` runs under `timeout ... -s KILL` with `retry=0` (`bins/agent/src/lib.rs:64-77,154-158`), so a missing entry parks workspaces in `HomeNotReady` rather than wedging the agent on a `hard` mount — but it belongs in the checklist, not in prose in another file.

Add to `deploy/k3s/README.md`, in the node-join section, as a numbered step immediately before the step that starts the agent on the new node:

```markdown
# N. The new node's flannel /32, BEFORE its agent first mounts. `system-netpol.yaml` allow-lists
#    NFS (2049) by node and flannel address; a missing entry is not an error anyone sees — the
#    mount times out and every workspace on that node parks in `HomeNotReady`.
ssh <newnode> 'ip -4 addr show flannel.1 | awk "/inet /{print \$2}"'
# Add that /32 to the `zerofs-nfs-from-agents` ingress in deploy/k3s/system-netpol.yaml, commit,
# then:
KUBECONFIG=.local/k3s.yaml kubectl apply -f deploy/k3s/system-netpol.yaml
```

- [ ] **Step 8: Full gate**

Run: `cargo test --workspace -- --test-threads=1; echo exit=$?`
Expected: `exit=0`.
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: no new warning in `crates/core/src/metrics.rs`.

- [ ] **Step 9: Commit — three commits, not one**

The Rust change, the RBAC change and the docs change are independently reviewable:

```bash
git add crates/core/src/metrics.rs
git commit -m "Drop the dead vol-agent metrics class"

git add deploy/k3s/api-rbac.yaml
git commit -m "Drop watch and update from the api's workspaces grant"

git add deploy/k3s/workspace-admission.yaml deploy/k3s/README.md
git commit -m "Write down the vol hostPath ceiling and the flannel join step"
```

---

### Task 6: Pin first-party images by digest and repin to HEAD

**Spec:** server-deploy.md Minors 4 and 5. First-party images are pinned by commit-SHA tag while third-party ones in the same files are digest-pinned — a GHCR tag is mutable, so a re-pushed `:<sha>` silently changes what an `IfNotPresent` node pulls next. And every tier is pinned to `1f24e39c` while HEAD is `4c7e94c9`, so the `services: delete` RBAC widening is live ahead of the code that uses it — which is exactly the ordering Task 1 must land under.

**Files:**
- Modify: `deploy/pin.sh`
- Modify (by running it): `deploy/rustic-git.yaml:58,398,579`, `deploy/k3s/agent-daemonset.yaml:114`, `deploy/k3s/gateway.yaml:131`, `deploy/rustic-git-web.yaml:49`

**Interfaces:**
- Consumes: Task 1's admission fence must be APPLIED to the cluster before the agent image that deletes Services rolls. Do not run this task before Task 1 and Task 2 are applied.
- Produces: image references of the form `ghcr.io/<org>/<image>:<sha>@sha256:<digest>`.

- [ ] **Step 1: Read `deploy/pin.sh` before changing it**

It already calls the GHCR API to prove a package exists for the SHA (a SHA with no package is refused, because CI only builds an image when that commit's test job passed). The digest is in the response it already fetches, or one `imagetools` call away. Do not add a new dependency or a second auth path.

- [ ] **Step 2: Resolve the digest**

In `deploy/pin.sh`, where each image reference is written, resolve the SHA to a digest first:

```bash
# A GHCR tag is mutable: a re-pushed :<sha> changes what an IfNotPresent node pulls next, which
# is exactly the silent-rollback the third-party images in these same files are already
# digest-pinned against. The tag stays for legibility; the digest is what actually runs.
digest_of() {
  docker buildx imagetools inspect --format '{{json .Manifest.Digest}}' "$1" \
    | tr -d '"' \
    || { echo "could not resolve a digest for $1" >&2; exit 1; }
}
```

and write `:<sha>@sha256:<digest>` in place of `:<sha>`. If `docker buildx` is not available on the machine that runs `pin.sh`, take the digest from the GHCR API response the script already has — one code path, not two; pick whichever the script is already authenticated for and say so in a comment.

- [ ] **Step 3: Make the rewrite match both shapes**

The sed/awk that rewrites the pins currently matches `:<40 hex>`. After this change it must also match an already-digest-pinned reference, or the second run leaves `:<sha>@sha256:<old>@sha256:<new>`. Match `:[0-9a-f]{40}(@sha256:[0-9a-f]{64})?` and replace the whole thing.

- [ ] **Step 4: Test the script on the current pins, twice**

```bash
./deploy/pin.sh 4c7e94c9 <web-sha>
git diff --stat
```
Expected: every pin in `deploy/` rewritten to `:4c7e94c9@sha256:…` (web from the web SHA — `web.yml` only runs when `web/**` changed, so the two images do NOT move in lockstep; pin each yaml to the last SHA that actually built that image).

Then run it a second time with the same arguments:
```bash
./deploy/pin.sh 4c7e94c9 <web-sha>
git diff --stat
```
Expected: NO further change. Idempotence is what proves Step 3's pattern is right.

- [ ] **Step 5: Verify a bad SHA is still refused**

```bash
./deploy/pin.sh 0000000000000000000000000000000000000000; echo exit=$?
```
Expected: non-zero, with the existing "no package for that SHA" message. A red commit has no package at all, and a repin to it is an ImagePullBackOff — the refusal is the guard against that, so do not let the digest lookup swallow it.

- [ ] **Step 6: Commit the pins**

```bash
git add deploy/pin.sh deploy/rustic-git.yaml deploy/rustic-git-web.yaml deploy/k3s/agent-daemonset.yaml deploy/k3s/gateway.yaml
git commit -m "Pin first-party images by digest and repin every tier to HEAD"
```

- [ ] **Step 7: Roll, in the right order**

Per `CLAUDE.md`'s deploy flow and `deploy/k3s/README.md`: the admission policy and the RBAC change (Tasks 1 and 2) are applied by hand on k3s FIRST, then `deploy/roll.sh`. A yaml roll must never outrun its image repin — the worker liveness probe counts per-lane heartbeat files and the web probes hit `/api/health`. Expect the first registry request to a moved image to 500 once (the known fenced-handle gap).

---

### Task 7: Delete the `crates/api` re-export shims

**Spec:** audit.md — "`crates/api/src/lib.rs`'s three re-export shim modules exist only 'to keep every call site unchanged' after the code moved to `core`/`storage`. Rename the call sites once; the shim is a permanent indirection paying for a one-time sed."

**Files:**
- Modify: `crates/api/src/lib.rs:15-30` (the `pub(crate) use` block, the `mod auth` shim, the `pub(crate) use ... as proxy` alias)
- Modify: every file under `crates/api/src/` that names `crate::auth::`, `crate::proxy::`, or one of the re-exported paths

**Interfaces:**
- Consumes: nothing.
- Produces: call sites name the real crates — `rustic_git_core::httpx::bearer_token`, `rustic_git_storage::auth::authorize`, `rustic_git_core::peer::PEER_HEADER`, `rustic_git_storage::store::Store`, and so on. Nothing named `crate::auth`, `crate::proxy`, `crate::store`, `crate::index`, `crate::directory` remains in this crate.

- [ ] **Step 1: Inventory the call sites**

```bash
cd crates/api/src
grep -rho "crate::auth::[a-z_]*"   . | sort | uniq -c
grep -rho "crate::proxy::[A-Za-z_]*" . | sort | uniq -c
for s in err hex jwt Result cache events index ownership store directory; do
  printf "%s %s\n" "$s" "$(grep -rho "crate::$s\b" . | wc -l)"
done
```
Expected roughly: `auth` 9, `proxy` 20, `directory` 32, `index` 15, `store` 12, `jwt` 6, `Result` 4, `err` 3, `ownership` 2, `cache` 1, `events` 1, `hex` 1. Around 105 sites, all mechanical. Save this output — it is your completeness check for Step 4.

- [ ] **Step 2: Rewrite the call sites**

Run from the repo root. Order matters: the longest prefixes first, so `crate::auth::` is not caught by a later `crate::a…` rule.

```bash
cd crates/api/src
# The axum-dependent header helpers live in core::httpx; the pure ones in storage::auth.
grep -rl 'crate::auth::' . | xargs sed -i '' \
  -e 's/crate::auth::bearer_token/rustic_git_core::httpx::bearer_token/g' \
  -e 's/crate::auth::basic_token/rustic_git_core::httpx::basic_token/g' \
  -e 's/crate::auth::basic_user_names/rustic_git_core::httpx::basic_user_names/g' \
  -e 's/crate::auth::unauthorized/rustic_git_core::httpx::unauthorized/g' \
  -e 's/crate::auth::/rustic_git_storage::auth::/g'
grep -rl 'crate::proxy::' . | xargs sed -i '' -e 's/crate::proxy::/rustic_git_core::peer::/g'
grep -rl 'crate::' . | xargs sed -i '' \
  -e 's/crate::cache::/rustic_git_storage::cache::/g' \
  -e 's/crate::events::/rustic_git_storage::events::/g' \
  -e 's/crate::index::/rustic_git_storage::index::/g' \
  -e 's/crate::ownership::/rustic_git_storage::ownership::/g' \
  -e 's/crate::store::/rustic_git_storage::store::/g' \
  -e 's/crate::directory::/rustic_git_pulls::directory::/g' \
  -e 's/crate::jwt::/rustic_git_core::jwt::/g' \
  -e 's/crate::hex::/rustic_git_core::hex::/g' \
  -e 's/crate::err(/rustic_git_core::err(/g'
```

`crate::Result` is a bare type name, not a path prefix — handle it by hand: replace the `pub(crate) use rustic_git_core::{… Result}` with a `use rustic_git_core::Result;` at the top of each of the four files that name it, or spell it `rustic_git_core::Result` inline. Whichever reads better in that file; do not introduce a new alias.

- [ ] **Step 3: Delete the shims**

Remove from `crates/api/src/lib.rs`:

```rust
pub(crate) use rustic_git_core::{err, hex, jwt, Result};
pub(crate) use rustic_git_storage::{cache, events, index, ownership, store};
pub(crate) use rustic_git_pulls::directory;
```

the whole `pub(crate) mod auth { … }` block with its comment, and:

```rust
pub(crate) use rustic_git_core::peer as proxy;
```

with its comment. The comments explained the indirection; with the indirection gone they explain nothing, so they go too. `use crate::cache::Cache;`, `use crate::events::Kind;` and `use crate::store::Store;` further down become `use rustic_git_storage::cache::Cache;` etc.

- [ ] **Step 4: Confirm nothing is left**

```bash
grep -rn "crate::auth::\|crate::proxy::\|crate::store::\|crate::index::\|crate::directory::\|crate::cache::\|crate::events::\|crate::ownership::\|crate::jwt::\|crate::hex::" crates/api/src
```
Expected: empty. `crate::` still legitimately names this crate's OWN modules (`crate::browse::Membership`, `crate::gpg`) — leave those.

- [ ] **Step 5: Build and test**

Run: `cargo test -p rustic-git-api; echo exit=$?`
Expected: `exit=0`. Then the full gate:
Run: `cargo test --workspace -- --test-threads=1; echo exit=$?`
Expected: `exit=0`.
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: no new warning. A `unused_imports` in `lib.rs` means a `use` line survived its last user — delete it.

- [ ] **Step 6: Commit**

```bash
git add crates/api/src
git commit -m "Name the real paths instead of re-export shims in the api crate"
```

Body: the shims existed to keep call sites unchanged after code moved to `core`/`storage`; that was a one-time cost paid permanently. ~105 sites, mechanical, no behaviour change.

---

### Task 8: Delete the two unwired benchmark examples

**Spec:** audit.md — "`examples/views_bench.rs` and `examples/tree_bench.rs` — referenced by no CI job, script, doc or CLAUDE.md; they compile on every `cargo test`."

**Decision, made by reading them** (the scope note asked for one): delete. Both are 35–37 line `fn main()`s that take a repo path and a commit oid as argv, call `rustic_git_git::browse::{files_at, last_changes, log}` and `println!` a `Instant::elapsed`. They are not benchmarks in any harness sense — no criterion, no baseline, no threshold, nothing that can fail. They answered a one-off question in `views_bench.rs`'s own words ("this is what decides whether the work belongs on a server or in the browser"), and that question is decided: the work is on the server. Wiring them into a documented `cargo bench` would mean adding criterion (a new dependency), a `[[bench]]` section and a CI job for numbers nobody reads. Git keeps them if the question comes back.

**Files:**
- Delete: `examples/views_bench.rs`, `examples/tree_bench.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: nothing. `examples/` becomes empty; remove the directory if git leaves it.

- [ ] **Step 1: Prove nothing references them**

```bash
grep -rn "views_bench\|tree_bench" --include=* . | grep -v "^./target" | grep -v "^./docs/superpowers"
grep -n "\[\[example\]\]\|bench" Cargo.toml
```
Expected: no hits outside the two files themselves and the review docs. No `[[example]]` section, no `[[bench]]` section.

- [ ] **Step 2: Delete**

```bash
git rm examples/views_bench.rs examples/tree_bench.rs
```

- [ ] **Step 3: Confirm the workspace still builds and got smaller**

Run: `cargo test --workspace -- --test-threads=1; echo exit=$?`
Expected: `exit=0`, and the build no longer compiles two example targets.

- [ ] **Step 4: Commit**

```bash
git commit -m "Delete the two unwired browse benchmarks"
```

Body: no CI job, script or doc referenced them; they compiled on every `cargo test` to answer a question already decided. `git log` has them if the question returns.

---

### Task 9: Make `Directory`'s methods required

**Spec:** audit.md — "every default is the 'unwired' answer and the one production impl overrides all of them; only test stubs use the defaults, and a partial stub silently reads as a live-but-empty directory."

The file's own doc comment states the trap: `teams_for`'s default returns an empty `Vec`, which `resolve_new_owner` reads as "asked and answered", so a partial stub gets a 403 "not a member" where no directory at all gets a 503. That is a test lying about production, which is worse than a compile error.

**Files:**
- Modify: `crates/workspaces/src/api.rs:60-80` — `trait Directory`'s three defaulted methods (`teams_for`, `is_live`, `for_owner`) and the paragraph of doc comment about defaults
- Modify: `crates/workspaces/tests/api_teams.rs:20`, `crates/workspaces/tests/api_user.rs:137,838,995,1017`, `crates/workspaces/src/api.rs:2532` — the six stubs
- Not modified: `bins/api/src/main.rs:26` — the production `Dir` adapter already implements all of them

**Interfaces:**
- Consumes: nothing.
- Produces: `trait Directory` with three required methods, unchanged signatures:
  - `async fn teams_for(&self, user: &str) -> Vec<String>`
  - `async fn is_live(&self, jti: &str) -> bool`
  - `async fn for_owner(&self, owner: &str) -> Option<OwnerMaterial>`

- [ ] **Step 1: Strip the default bodies**

In `crates/workspaces/src/api.rs`, turn each defaulted method into a declaration. Keep every doc comment — `is_live`'s "`false` refuses the token, which is what an unwired directory must do" and `for_owner`'s "`None` when the lookup FAILED" are the two facts a stub author needs.

```rust
    /// Every team slug `user` belongs to. Called once per request, no cache —
    /// ponytail: an in-process cache would cut the N+1 here, add one if this ever shows up hot.
    async fn teams_for(&self, user: &str) -> Vec<String>;

    /// Is this CLI login still valid? A `cli` JWT carries a `jti` whose row in the directory IS
    /// the revocation list — the same rule `crates/api`'s `user_identity` enforces. `false`
    /// refuses the token, which is what an unwired directory must do: a 30-day token nobody can
    /// cancel is the worse failure.
    async fn is_live(&self, jti: &str) -> bool;

    /// The owner's ssh keys and git identity. `None` when the lookup FAILED — distinct from `Some`
    /// with an empty `authorized_keys`, which is a user with no keys and is written as an empty
    /// file.
    async fn for_owner(&self, owner: &str) -> Option<OwnerMaterial>;
```

Replace the "Every method defaults to the UNWIRED answer…" paragraph in the trait's doc comment with:

```rust
/// Every method is REQUIRED. A defaulted one made a partial test stub read as a live-but-empty
/// directory — `teams_for` returning an empty Vec is "asked and answered" to `resolve_new_owner`,
/// which is a 403 "not a member", where no directory at all is a 503. A stub must say which it
/// means.
```

- [ ] **Step 2: Run the build and let the compiler list the stubs**

Run: `cargo test -p rustic-git-workspaces --no-run; echo exit=$?`
Expected: FAIL, `error[E0046]: not all trait items implemented` at each of the six stub sites. That list IS the work item for Step 3.

- [ ] **Step 3: Fill in each stub explicitly**

For each stub, add only the missing methods, each returning the answer that stub actually means — which is the same value the default returned, now written where a reader can see it:

```rust
    // This stub exercises team membership only; a CLI token is not part of its case, and an
    // unwired revocation list must refuse rather than admit.
    async fn is_live(&self, _jti: &str) -> bool {
        false
    }
    // No keys in this case: `None` is "the lookup failed", which is what an unwired directory is.
    async fn for_owner(&self, _owner: &str) -> Option<OwnerMaterial> {
        None
    }
```

Adjust the comment per stub to name that stub's actual case (`StubMembership`, `StubCliTokens`, `StubKeys`, `KeyTeams`, and the in-module `Stub` at `api.rs:2532`). Do not paste one comment six times.

- [ ] **Step 4: Test**

Run: `cargo test -p rustic-git-workspaces; echo exit=$?`
Expected: `exit=0`. A test that now FAILS is the finding paying off — it was relying on a default that lied. Read what it asserts before changing the stub's answer: if it wanted "no teams", `Vec::new()` is right; if it wanted "no directory", the `ApiState.directory` should have been `None` in the first place.

- [ ] **Step 5: Full gate**

Run: `cargo test --workspace -- --test-threads=1; echo exit=$?`
Expected: `exit=0`.
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: no new warning.

- [ ] **Step 6: Commit**

```bash
git add crates/workspaces/src/api.rs crates/workspaces/tests/api_teams.rs crates/workspaces/tests/api_user.rs
git commit -m "Make every Directory method required"
```

---

### Task 10: `rfc3339` via chrono, and delete the gateway's re-export module

**Spec:** audit.md — `rfc3339(ms)` reaches through `mongodb::bson::DateTime` to format an instant in a module with nothing else to do with mongo; and `bins/gateway/src/lib.rs` is four lines of `pub mod` plus a re-export.

Two unrelated one-file cuts, batched because neither carries its own test cycle.

**Files:**
- Modify: `crates/api/src/credentials.rs:677`
- Modify: `crates/api/Cargo.toml` (add `chrono = { workspace = true }` if absent)
- Delete: `bins/gateway/src/lib.rs`
- Modify: `bins/gateway/src/main.rs` and any other caller of `rustic_git_gateway::{app, Gateway}`
- Not modified: `src/lib.rs` — see Step 6

**Interfaces:**
- Consumes: nothing.
- Produces: `fn rfc3339(ms: i64) -> String` — same signature, same four call sites (`credentials.rs:626,667,719,720`), same output shape. `bins/gateway`'s `app` and `Gateway` are named as `tunnel::app` / `tunnel::Gateway`.

- [ ] **Step 1: Write the failing test for `rfc3339`**

Add to `crates/api/src/credentials.rs`'s test module (create one if there is none):

```rust
#[test]
fn rfc3339_formats_epoch_millis_and_survives_a_nonsense_input() {
    // The four call sites feed it `timestamp_millis()` and `exp * 1000`; a bad value must not
    // panic a route, it must answer the empty string the old bson path answered.
    assert_eq!(super::rfc3339(0), "1970-01-01T00:00:00+00:00");
    assert_eq!(super::rfc3339(i64::MAX), "");
}
```

- [ ] **Step 2: Run it against the current bson implementation**

Run: `cargo test -p rustic-git-api rfc3339_formats -- --nocapture; echo exit=$?`
Expected: it may PASS or FAIL depending on bson's exact spelling (`+00:00` vs `Z`). Record the actual output — that is the contract the replacement must match. If bson emits `Z`, change the assertion to `Z` and make chrono match by formatting with `to_rfc3339_opts(SecondsFormat::Secs, true)`. **Do not change what the API answers**; the web app parses these.

- [ ] **Step 3: Replace the implementation**

```rust
/// Epoch milliseconds as RFC3339. One spelling for every instant these routes answer with.
/// `chrono`, not `mongodb::bson::DateTime` — nothing else in this module is about mongo, and an
/// out-of-range instant answers empty here exactly as bson's fallible conversion did.
fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.to_rfc3339()).unwrap_or_default()
}
```

If Step 2 recorded a `Z`-suffixed output, use instead:

```rust
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
```

Add `chrono = { workspace = true }` to `crates/api/Cargo.toml` if it is not already there (`chrono = "0.4"` is already in the workspace `[workspace.dependencies]` at `Cargo.toml:102`). Do NOT remove `mongodb` from that Cargo.toml — the directory types still need it; check with `grep -n mongodb crates/api/src/*.rs` and only remove the dependency if that grep is empty.

- [ ] **Step 4: Test**

Run: `cargo test -p rustic-git-api; echo exit=$?`
Expected: `exit=0`, with the assertion matching the string Step 2 recorded.

- [ ] **Step 5: Delete the gateway's lib shim**

`bins/gateway/src/lib.rs` is:

```rust
pub mod resolve;
pub mod tunnel;

pub use tunnel::{app, Gateway};
```

Find the callers first:

```bash
grep -rn "rustic_git_gateway::" --include=*.rs . | grep -v "^./target"
```

If every caller is inside `bins/gateway` itself, delete `lib.rs`, declare `mod resolve; mod tunnel;` in `main.rs`, and name `tunnel::app` / `tunnel::Gateway` at the two call sites. If a test binary or another crate imports `rustic_git_gateway::app`, keep `lib.rs` with only `pub mod resolve; pub mod tunnel;` and change that caller to `rustic_git_gateway::tunnel::app` — the re-export goes either way; the module declarations may have to stay.

- [ ] **Step 6: Leave `src/lib.rs` alone**

The audit names `src/lib.rs` (a one-line doc comment) in the same item. It exists so the root package `rustic-git-tests` has a lib target to host `tests/*.rs`, which is the whole integration suite. Cargo needs a target there; deleting the file would move every integration test. Not a cut — record the decision in the commit body and take no action.

- [ ] **Step 7: Full gate**

Run: `cargo test --workspace -- --test-threads=1; echo exit=$?`
Expected: `exit=0`.
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: no new warning.

- [ ] **Step 8: Commit — two commits**

```bash
git add crates/api/src/credentials.rs crates/api/Cargo.toml
git commit -m "Format instants with chrono instead of reaching through bson"

git add bins/gateway
git commit -m "Name the gateway's tunnel module directly"
```

---

### Task 11: Document each tunable's real setter

**Spec:** audit.md — "tunables with a default, one reader and no setter anywhere in `deploy/` — `RUSTIC_GIT_MAX_LAYER`, `RUSTIC_GIT_ALLOW_MEM_FLEET`, `WS_BASE_PACKAGES`, `RUSTIC_GIT_LOG_FORMAT`. Inline the constant; add the env read back the day someone needs to change it."

**The audit's premise does not survive the grep — and the scope note said to decide per item.** Each of the four HAS a setter; three of them just are not in a Deployment's `env:` block. So the cut is not deletion, it is naming the setter at the read, so the next audit does not re-file this:

| Tunable | Setter found | Action |
|---|---|---|
| `RUSTIC_GIT_MAX_LAYER` | `tests/registry_limits.rs:11` sets it — its own test binary exists BECAUSE `max_layer()` is a process-wide `OnceLock` | Keep the read; name the test at the read |
| `RUSTIC_GIT_ALLOW_MEM_FLEET` | `crates/storage/src/config.rs:191-195` (its own unit test) and the in-process test fleet the refusal message names | Keep; name it |
| `WS_BASE_PACKAGES` | `deploy/k3s/agent-daemonset.yaml:192` — it IS set in `deploy/`, so the audit item is simply wrong here | Keep; note the daemonset |
| `RUSTIC_GIT_LOG_FORMAT` | `deploy/alerts.md:5` documents `RUSTIC_GIT_LOG_FORMAT=json` on any pod as the way to get structured logs | Keep; note alerts.md |

**Files:**
- Modify (doc comments only): `crates/registry/src/blobs.rs:22-23`, `crates/storage/src/config.rs:130-137`, `bins/agent/src/nix.rs:56-63`, `crates/core/src/log.rs:13`

**Interfaces:**
- Consumes: nothing. **Produces: no code change at all** — every `std::env::var` read stays exactly as it is.

- [ ] **Step 1: Re-run the evidence yourself before writing it down**

```bash
grep -rn "RUSTIC_GIT_MAX_LAYER\|ALLOW_MEM_FLEET\|WS_BASE_PACKAGES\|RUSTIC_GIT_LOG_FORMAT" \
  --include=*.rs --include=*.yaml --include=*.sh --include=*.md . \
  | grep -v "^./target" | grep -v "^./docs/superpowers"
```
Expected: the four setters in the table above. If any one of them turns out to have NO setter, that one gets the audit's treatment instead — delete the `std::env::var` call, keep the constant, and say in the doc comment that the env read comes back the day someone needs to change it in a running cluster.

- [ ] **Step 2: Name the setter at each read**

`crates/registry/src/blobs.rs`, on `max_layer()`:

```rust
/// Largest single layer accepted, checked against the body's size BEFORE it is stored: an
/// unbounded push must not be able to fill a node's disk. `RUSTIC_GIT_MAX_LAYER` overrides it and
/// has exactly one setter — `tests/registry_limits.rs`, which is its own test binary precisely
/// because this is a process-wide `OnceLock`. No Deployment sets it; the default is the ceiling
/// in production and changing it means a code change, which is the intent.
```

`crates/storage/src/config.rs`, on the `mem://` branch — append to the existing comment:

```rust
    // Set only by this module's own unit test and by an in-process test fleet. Nothing in
    // `deploy/` sets it, and nothing should: a real fleet on `mem://` is the two-writer bug.
```

`bins/agent/src/nix.rs`, on `DEFAULT_BASE_PACKAGES` / the read at `:63`:

```rust
/// `WS_BASE_PACKAGES`, whitespace-separated. SET IN PRODUCTION: `deploy/k3s/agent-daemonset.yaml`
/// passes it, so the constant below is the fallback, not the value the cluster runs.
```

`crates/core/src/log.rs:13`, on the module doc:

```rust
//! `RUSTIC_GIT_LOG_FORMAT=json` switches every binary to one JSON object per line, so a log
//! aggregator gets fields instead of a string. Not set by any manifest by default —
//! `deploy/alerts.md` documents setting it on a pod when someone is debugging one.
```

- [ ] **Step 3: Gate**

Run: `cargo test --workspace -- --test-threads=1; echo exit=$?`
Expected: `exit=0` (comment-only, but the doc comments are on public items and a malformed doc link fails the build).
Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: no new warning.

- [ ] **Step 4: Commit**

```bash
git add crates/registry/src/blobs.rs crates/storage/src/config.rs bins/agent/src/nix.rs crates/core/src/log.rs
git commit -m "Name each tunable's real setter at the read"
```

Body: the audit filed these as "no setter anywhere in deploy/"; three have a setter outside `deploy/` and `WS_BASE_PACKAGES` is set in `agent-daemonset.yaml:192`. Deleting the reads would have broken `tests/registry_limits.rs` and the in-process test fleet. Naming the setter is the fix.

---

## Self-Review

**1. Spec coverage.** Every numbered finding in `server-deploy.md`, and every scoped audit cut:

| Item | Task |
|---|---|
| Important 1 — DELETE fence | 1 |
| Important 2 — dead `apps/deployments` grant | 2 |
| Important 3 — `tests/routing.rs` flake | 3 |
| Minor 4 — digest pinning | 6 |
| Minor 5 — pins lag HEAD | 6 |
| Minor 6 — hostPath `vol/` not owner-scoped | 5 |
| Minor 7 — api-rbac `watch`/`update` | 5 |
| Minor 8 — stale `git-token` name | 2 (same file's table) |
| Minor 9 — `/vol-agent/` metrics class | 5 |
| Minor 10 — flannel `/32` runbook step | 5 |
| Cleanup 11 — `VolExt`'s four methods | 4 |
| Cleanup 12 — `browse_api/volumes.rs` FROZEN | 4, Step 7 (recorded, no action) |
| Audit — `crates/api` re-export shims | 7 |
| Audit — `examples/*_bench.rs` | 8 (decision: delete, reasoning given) |
| Audit — `Directory`'s four default bodies | 9 (three defaults; the fourth trait item, `vol_db`-style, has no default — see note) |
| Audit — `rfc3339` via bson | 10 |
| Audit — `bins/gateway/src/lib.rs`, `src/lib.rs` | 10 (gateway deleted; `src/lib.rs` kept, reason recorded) |
| Audit — tunables with no setter | 11 (evidence contradicts the premise; documented per item) |

Note on Task 9: the scope note says "`Directory`'s four default method bodies"; the trait as it stands at `crates/workspaces/src/api.rs:60-80` has THREE defaulted methods (`teams_for`, `is_live`, `for_owner`). Task 9 Step 2 makes the compiler enumerate them — if a fourth exists at execution time, it gets the same treatment in the same commit.

**2. Placeholder scan.** No "TBD", no "add error handling", no "similar to Task N". Every code step carries the actual code or the actual command. Two steps deliberately branch on a value only the cluster or the compiler can produce (Task 10 Step 2's `Z` vs `+00:00`, Task 9 Step 3's stub list) — both name exactly how to obtain the value and what to do with each answer.

**3. Type consistency.** `App::election_tick`, `App::renew_once`, `App::claim` used in Task 3 match `crates/app/src/lib.rs:232,708,593`. `VolExt`'s surviving four methods in Task 4 match the names Task 4's grep proves are called. `rfc3339(ms: i64) -> String` is unchanged across Task 10. `Directory`'s three signatures in Task 9 are copied verbatim from the trait.

## Execution order

Tasks 1 → 2 → 3 → 4 → 5 → 6, then 7 → 8 → 9 → 10 → 11 in any order (each touches a disjoint file set). Task 6 must not roll before Tasks 1 and 2 are APPLIED to k3s: the RBAC widening for `services: delete` is already live and the code that uses it is what Task 6 ships.
