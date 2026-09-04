# Workspaces crate + `/v1` review fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land every finding in `docs/superpowers/reviews/2026-09-03-details/workspaces-api.md` plus the audit cuts that belong to this crate, so `/v1` stops destroying other owners' snapshots, stops authorizing on labels, stops handing out Azure account names, and loses the Cosmos tier entirely.

**Architecture:** `crates/workspaces` is the library (`crd.rs` = the CRDs, `api.rs` = every `/v1` handler, `k8s.rs` = the object generators, `model.rs` = the wire docs); `bins/api` wires it. Nothing here changes the control-plane shape: Kubernetes stays the reconcile substrate and the CRDs stay the truth. The two structural moves are (a) `Region` becomes a cluster-scoped CRD written by `/v1/regions`, which deletes the whole Cosmos/`MetaStore` tier, and (b) `api.rs` splits into one module per resource with a single `mine()` scope filter, so "narrow by label, decide on `spec.owner`" exists once instead of seven times.

**Tech Stack:** Rust 2021, axum 0.8-style handlers (`Result<T, Response>`), `kube`/`k8s-openapi` with `#[derive(CustomResource)]`, `crates/workspaces/src/kube_test.rs`'s recorder-backed mock `kube::Client`, integration tests in `crates/workspaces/tests/*.rs`.

**Spec:** `docs/superpowers/specs/2026-09-03-durable-snapshots-design.md` and `docs/superpowers/specs/2026-09-03-snapshot-state-design.md` (behaviour authority); `docs/superpowers/reviews/2026-09-03-details/workspaces-api.md` and `.../audit.md` (the findings); `docs/superpowers/reviews/2026-09-03-codebase-review.md` (the order).

## Global Constraints

- **Commit subjects are imperative sentence case with no tool attribution.** No `Co-Authored-By`, no `Generated with`, no task numbers in the subject.
- **Comments explain WHY, never what.** Match the density of `bins/server/src/router/route.rs`. A comment that restates the line below it is a review finding.
- **Keep every `// ponytail:` marker** you edit near; a new deliberate shortcut gets one naming its ceiling and upgrade path.
- **Vocabulary (from the durable-snapshots design):** *workspace*, *environment*, *push*, *snapshot* (`spec.transient: false`), *sync point* (`spec.transient: true`), *volume*, *worktree*. There is no "commit". The one exception is `VolumeSource::CloneOf { commit }`, whose **field name is on stored CRs** and must not be renamed.
- **Never authorize on a label.** A label selector is an index; the decision reads `spec.owner`. (CLAUDE.md.)
- **`/v1` writes spec only, never status.** Status is the node controllers'.
- **Manifest is generated:** any `crd.rs` change to a spec/status struct or a `#[kube(...)]` attribute requires `CRD_REGEN=1 cargo test -p kloudlite-git-workspaces --test crd_yaml` and the regenerated `deploy/k3s/crds.yaml` in the same commit.
- **Gates, run unpiped, after every task:**
  ```
  cargo test -p kloudlite-git-workspaces -p kloudlite-git-agent-bin -- --test-threads=1; echo exit=$?
  cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?
  ```
  and, when `crd.rs` changed:
  ```
  CRD_REGEN=1 cargo test -p kloudlite-git-workspaces --test crd_yaml; echo exit=$?
  ```
- **Interfaces shared with the sibling plans** (do not implement them here, but do not contradict them):
  - `VolumeSource::RestoreOf` **disappears from the enum** (Task 11 here). The agent plan removes its match arms in `bins/agent/src/controller/volume.rs:268,269,593` and `engine::ops::RESTORE_OF_GONE` + the `RestoreMechanismGone` reason.
  - `Workspace.live_state` — the field name — is deleted from `crates/workspaces/src/model.rs` here (Task 14); the web plan deletes the matching TS declaration at `web/apps/web/src/lib/api.ts:709`.

---

## File Structure

| file | responsibility after this plan |
|---|---|
| `crates/workspaces/src/api/mod.rs` | `ApiState`, `Directory`, `router()`, `caller`/`unauthorized`/`kube_err`, region routes |
| `crates/workspaces/src/api/scope.rs` | `caller_owners`, `may_act_on`, `teams_for`, `owned_by`/`owned_in`, `owner_set_selector`, `Owned`, `mine()`, `my_ws`, `find_env` |
| `crates/workspaces/src/api/workspaces.rs` | create/list/get/delete/start/stop/attach/detach/packages/clone/restore/ssh |
| `crates/workspaces/src/api/environments.rs` | the environment twins + `restore_env_in_place` |
| `crates/workspaces/src/api/volumes.rs` | `list_volumes`, history, refs, `delete_volume`, `delete_snapshot`, `snapshot_rows`, `parents_of_volume`, `live_parents` |
| `crates/workspaces/src/api/push.rs` | `push_ws`, `push_env`, `create_snapshot`, `clone_base`, `refuse_cut_in_flight` |
| `crates/workspaces/src/crd.rs` | adds `Region`; loses `VolumeSource::RestoreOf`; `hex_prefix` becomes `hex::encode` |
| `crates/workspaces/src/crd/names.rs` | `ws_namespace`, `env_namespace`, `binding_name`, `dns_label`, `pair_tail` |
| `crates/workspaces/src/k8s.rs` | adds `resource_quota` |
| `crates/workspaces/src/model.rs` | loses `Region`, loses `Workspace.live_state` |
| deleted | `crates/workspaces/src/cosmos.rs`, `crates/workspaces/src/store.rs`, `crates/workspaces/tests/meta_store.rs` |

Tasks 1–8 and 11–16 edit `api.rs` as one file; Task 10 performs the split. That order is deliberate: the split is mechanical and reviewable only once the behaviour changes have landed.

---

### Task 1: `delete_volume` / `delete_snapshot` must count snapshots unfiltered (C1)

**Files:**
- Modify: `crates/workspaces/src/api.rs:2264-2280` (`delete_volume`), `:2306-2349` (`delete_snapshot`), `:2354-2379` (the two snapshot listings)
- Test: `crates/workspaces/tests/api_volumes.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `async fn snapshots_on_volume(s: &ApiState, name: &str) -> Result<Vec<crd::Snapshot>, Response>` — every `Snapshot` with `spec.volume == name`, **no owner filter**, newest first. `commit_model_snapshots` / `commit_model_snapshots_maybe_empty` keep their current owner-filtered signatures and are still what `/history`, `/refs` and the "may this caller see this volume" probe use.

- [ ] **Step 1: Write the failing tests**

Add to `crates/workspaces/tests/api_volumes.rs` (the fixtures `push`, `snap_list`, `ws_list`, `env_list`, `server`, `token`, `delete`, `ok`, `SNAPS`, `API` already exist in that file):

```rust
/// A volume genuinely holds snapshots from more than one owner: a restore grafts the caller's new
/// workspace onto a team's volume, and `create_snapshot` stamps the PUSHING worktree's owner. The
/// owner-filtered list is what the caller may SEE; it must never be what decides whether the
/// volume is empty, or one team member's delete takes the team's whole history.
#[tokio::test]
async fn a_foreign_snapshot_on_the_volume_refuses_the_volume_delete() {
    let s = server(vec![
        kget(
            SNAPS,
            snap_list(vec![
                push("ws-1-a", "ws-1", "karthik", "2026-08-27T09:00:00Z"),
                push("ws-1-b", "ws-1", "alice", "2026-08-27T10:00:00Z"),
            ]),
        ),
        kget(format!("{API}/workspaces"), ws_list(vec![])),
        kget(format!("{API}/environments"), env_list(vec![])),
    ])
    .await;

    assert_eq!(delete(&s, &token(&s.jwt, "karthik"), "/v1/volumes/ws-1").await, 409);
    assert!(s.rec.calls().iter().all(|c| !c.starts_with("DELETE")), "nothing was deleted: {:?}", s.rec.calls());
}

/// The same split, from the other end: deleting my own last snapshot must not collect a volume
/// that still holds somebody else's.
#[tokio::test]
async fn a_foreign_snapshot_keeps_the_volume_after_my_last_snapshot_goes() {
    let s = server(vec![
        kget(
            SNAPS,
            snap_list(vec![
                push("ws-1-a", "ws-1", "karthik", "2026-08-27T09:00:00Z"),
                push("ws-1-b", "ws-1", "alice", "2026-08-27T10:00:00Z"),
            ]),
        ),
        kget(format!("{API}/workspaces"), ws_list(vec![])),
        kget(format!("{API}/environments"), env_list(vec![])),
        ok("DELETE", format!("{SNAPS}/ws-1-a")),
    ])
    .await;

    assert_eq!(delete(&s, &token(&s.jwt, "karthik"), "/v1/volumes/ws-1/snapshots/ws-1-a").await, 204);
    let deletes: Vec<String> = s.rec.calls().into_iter().filter(|c| c.starts_with("DELETE")).collect();
    assert_eq!(deletes, vec![format!("DELETE {SNAPS}/ws-1-a")], "the volume is alice's too: {deletes:?}");
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p kloudlite-git-workspaces --test api_volumes -- --test-threads=1; echo exit=$?`
Expected: FAIL — the first gets 204 instead of 409, the second records a second `DELETE .../volumes/ws-1`.

- [ ] **Step 3: Add the unfiltered listing and use it for the two destructive decisions**

In `crates/workspaces/src/api.rs`, beside `commit_model_snapshots_maybe_empty`:

```rust
/// Every snapshot on `volume`, whoever owns it — the same bias `parents_of_volume` takes, and for
/// the same reason: a restore or a shared clone puts another owner's snapshots on this volume, and
/// a decision that DESTROYS data must see them. The owner-filtered listing above stays what the
/// caller may read; this one is only ever counted, never returned.
async fn snapshots_on_volume(s: &ApiState, name: &str) -> Result<Vec<crd::Snapshot>, Response> {
    check_path_segment(name)?;
    let api: Api<crd::Snapshot> = Api::all(kube(s)?.clone());
    Ok(api
        .list(&ListParams::default().fields(&format!("spec.volume={name}")))
        .await
        .map_err(kube_err)?
        .items)
}
```

In `delete_volume`, after the existing `commit_model_snapshots` ownership probe and before `delete_volume_cr`:

```rust
    // Deleting the Volume CR cascades to every Snapshot on it, so a volume carrying somebody
    // else's push is not this caller's to collect — they may not even be able to see it.
    let owners: HashSet<String> = caller_owners(&s, &caller_id).await.into_iter().collect();
    if snapshots_on_volume(&s, &name).await?.iter().any(|sn| !owners.contains(&sn.spec.owner)) {
        return Err((
            StatusCode::CONFLICT,
            "this volume also holds snapshots owned by someone else; delete your own snapshots instead",
        )
            .into_response());
    }
```

In `delete_snapshot`, replace the post-delete `commit_model_snapshots_maybe_empty` re-read (`:2338`) with the unfiltered one and drop the owner term from `remaining`:

```rust
    let items = snapshots_on_volume(&s, &name).await?;
    let live = parents_of_volume(&s, &name).await.ok_or_else(kube_unavailable)?;
    let remaining = items.iter().any(|sn| {
        sn.name_any() != snapshot
            && sn.is_snapshot()
            && sn.status.as_ref().is_none_or(|st| st.phase != crd::Phase::Error)
    });
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p kloudlite-git-workspaces -p kloudlite-git-agent-bin -- --test-threads=1; echo exit=$?` then `cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?`
Expected: exit=0 both.

- [ ] **Step 5: Commit**

```bash
git add crates/workspaces/src/api.rs crates/workspaces/tests/api_volumes.rs
git commit -m "Count a volume's snapshots unfiltered before deleting anything"
```

---

### Task 2: A per-owner creation cap in `/v1` (C2, first half)

**Files:**
- Modify: `crates/workspaces/src/api.rs:538` (`create_ws`), `:1208` (`clone_ws`), `:1330` (`restore_ws`), `:1502` (`create_env`), `:1567` (`restore_env`)
- Modify: `crates/workspaces/src/model.rs` (the constant + env read)
- Test: `crates/workspaces/tests/api_user.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `pub fn max_per_owner() -> usize` in `crates/workspaces/src/model.rs` — reads `WS_MAX_PER_OWNER`, default `20`; and `async fn refuse_over_cap(s: &ApiState, c: &kube::Client, owner: &str) -> Result<(), Response>` in `api.rs`, which Task 3 reuses for the ResourceQuota sizing.

**Chosen defaults, and why:** `WS_MAX_PER_OWNER = 20`. A workspace pod requests 4 GiB / 2 vCPU (`PodResources::default()`, asserted in `k8s.rs:1801`), so 20 is 80 GiB and 40 vCPU of requests per person — more than any one person on this platform uses and well under a single k3s node's capacity, which is what makes it a runaway-loop stop rather than a product limit. It counts workspaces and environments **together**, under one owner label, because they cost the same node capacity and the same btrfs pool. **429**, not 409: the caller is not conflicting with a specific object, they are asking for too many — and the message says the number, so a person who legitimately needs more knows what to ask for.

- [ ] **Step 1: Write the failing test**

Add to `crates/workspaces/tests/api_user.rs`:

```rust
/// A `for` loop over POST /v1/workspaces used to reserve the cluster's whole schedulable memory
/// and fill the btrfs pool from one ordinary account. The cap is counted over workspaces AND
/// environments together — they cost the same node and the same pool.
#[tokio::test]
async fn creating_past_the_per_owner_cap_is_refused() {
    let many: Vec<Value> = (0..20).map(|i| ws_obj(&format!("ws-{i}"), "karthik")).collect();
    let s = server(vec![
        get(format!("{API}/workspaces"), json!({"apiVersion": "kloudlite-git.io/v1alpha1", "kind": "WorkspaceList", "metadata": {}, "items": many})),
        get(format!("{API}/environments"), json!({"apiVersion": "kloudlite-git.io/v1alpha1", "kind": "EnvironmentList", "metadata": {}, "items": []})),
        post(format!("{API}/workspaces"), ws_obj("ws-new", "karthik")),
    ])
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"name": "twenty-one", "region": "centralindia", "quota_gb": 20}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    let body = resp.text().await.unwrap();
    assert!(body.contains("20"), "the message states the limit: {body}");
    assert!(s.rec.sent("POST", &format!("{API}/workspaces")).is_empty(), "nothing written");
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p kloudlite-git-workspaces --test api_user creating_past_the_per_owner_cap -- --test-threads=1; echo exit=$?`
Expected: FAIL with `202` (or a taken-name 409), not 429.

- [ ] **Step 3: Implement the cap**

In `crates/workspaces/src/model.rs`:

```rust
/// How many workspaces plus environments one owner may have at once.
///
/// A runaway-loop stop, not a product limit: a workspace pod requests 4 GiB / 2 vCPU, so twenty is
/// 80 GiB and 40 vCPU of requests per person — more than anyone here runs, and far under what a
/// single `POST` loop reserved before this existed. Env-configurable because the number that is
/// obviously safe today is a cluster-capacity question tomorrow.
pub fn max_per_owner() -> usize {
    std::env::var("WS_MAX_PER_OWNER").ok().and_then(|v| v.parse().ok()).unwrap_or(20)
}
```

In `api.rs`, beside `refuse_taken_name`:

```rust
/// Refuse a create that would take this owner past their ceiling.
///
/// The two label-selected lists cost what `refuse_taken_name` already pays, and the DECISION reads
/// `spec.owner` (labels are a view): an object mislabelled onto someone else must not spend their
/// budget. Counted across both kinds — they share a node's memory and the pool.
async fn refuse_over_cap(s: &ApiState, c: &kube::Client, owner: &str) -> Result<(), Response> {
    let max = crate::model::max_per_owner();
    let lp = owned_by(owner);
    let ws = Api::<crd::Workspace>::all(c.clone()).list(&lp).await.map_err(kube_err)?;
    let envs = Api::<crd::Environment>::all(c.clone()).list(&lp).await.map_err(kube_err)?;
    let mine = ws.items.iter().filter(|w| w.spec.owner == owner).count()
        + envs.items.iter().filter(|e| e.spec.owner == owner).count();
    if mine >= max {
        // 429, not 409: nothing conflicts with a particular object — this account is asking for
        // more than it may hold. The number is in the message so the person can ask for a raise.
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            format!("you already have {mine} workspaces and environments; the limit is {max}"),
        )
            .into_response());
    }
    Ok(())
}
```

Call it in all five create paths, immediately after the owner/team is resolved and before anything is written:
- `create_ws` — after `refuse_taken_name(...)` at `:555`, as `refuse_over_cap(&s, c, &owner).await?;`
- `clone_ws` — after `refuse_taken_name(...)` at `:1217`
- `restore_ws` — after `refuse_taken_name(...)` at `:1351`
- `create_env` — after `resolve_new_owner(...)` at `:1514`, as `refuse_over_cap(&s, c, &owner).await?;`
- `restore_env` — after `resolve_new_owner(...)` at `:1600`

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p kloudlite-git-workspaces -p kloudlite-git-agent-bin -- --test-threads=1; echo exit=$?`
Expected: exit=0. Existing create tests supply an empty `WorkspaceList`; those that do **not** already stub `GET {API}/environments` will now 404 on it — add `get(format!("{API}/environments"), json!({"apiVersion": "kloudlite-git.io/v1alpha1", "kind": "EnvironmentList", "metadata": {}, "items": []}))` to `create_routes()` in `api_user.rs` and to any per-test route list that creates.

- [ ] **Step 5: Commit**

```bash
git add crates/workspaces/src/api.rs crates/workspaces/src/model.rs crates/workspaces/tests/api_user.rs
git commit -m "Cap how many workspaces and environments one owner may create"
```

---

### Task 3: A `ResourceQuota` per owner namespace as the backstop (C2, second half)

**Files:**
- Modify: `crates/workspaces/src/k8s.rs` (new `resource_quota`, next to `limit_range:138`)
- Modify: `bins/agent/src/binding.rs:100-110` (the `ensure` beside the LimitRange)
- Modify: `deploy/k3s/agent-rbac.yaml` (the header table and the `limitranges` rule at `:215`)
- Test: `crates/workspaces/src/k8s.rs` unit tests (beside `the_namespace_refuses_anything_larger_than_its_slot:1798`)

**Interfaces:**
- Consumes: `crate::model::max_per_owner()` from Task 2.
- Produces: `pub fn resource_quota(ns: &str, owner: &str, kind: &str, res: &PodResources, max_pods: usize) -> ResourceQuota`.

**Why both halves:** the `/v1` check is the one that gives a person a readable answer; the quota is what holds when an object is written by any other path — an operator with kubectl, a restored backup, a future handler that forgets. Same reasoning as `limit_range`'s own doc comment.

- [ ] **Step 1: Write the failing test**

Append to the `k8s.rs` test module:

```rust
/// The `/v1` count check is the readable refusal; this is the one that holds for a pod created by
/// any path. Sized from the SAME two numbers the slot and the cap are, so the three cannot drift.
#[test]
fn the_namespace_caps_aggregate_consumption_not_just_container_size() {
    let q = resource_quota("ws-alice", "alice", "workspace", &PodResources::default(), 20);
    let hard = q.spec.unwrap().hard.unwrap();
    assert_eq!(hard.get("pods").unwrap().0, "20");
    assert_eq!(hard.get("requests.cpu").unwrap().0, "40");
    assert_eq!(hard.get("requests.memory").unwrap().0, "80Gi");
    // Shared user namespace: no ownerReference, exactly as the LimitRange has none — deleting one
    // workspace must not drop the ceiling for every sibling.
    assert!(q.metadata.owner_references.is_none());
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p kloudlite-git-workspaces --lib the_namespace_caps_aggregate -- --test-threads=1; echo exit=$?`
Expected: FAIL — `resource_quota` not found.

- [ ] **Step 3: Write the generator and ensure it**

In `crates/workspaces/src/k8s.rs`, after `limit_range` (add `ResourceQuota`/`ResourceQuotaSpec` to the `k8s_openapi::api::core::v1` import list):

```rust
/// The namespace's aggregate ceiling: `LimitRange` bounds one container, this bounds all of them.
///
/// Sized as `max_pods` × the slot's DEFAULT REQUEST, because capacity is priced on the request
/// (see `PodResources::default`) — so this quota and `/v1`'s per-owner count refuse at the same
/// point instead of one silently shadowing the other. Requests only, never limits: bursting to the
/// limit is what the slot is for.
pub fn resource_quota(ns: &str, owner: &str, kind: &str, res: &PodResources, max_pods: usize) -> ResourceQuota {
    let cpu = res.cpu_request.trim_end_matches('m').parse::<f64>().unwrap_or(0.0);
    // `cpu_request` is either whole cores ("2") or millicores ("500m"); normalise to millicores so
    // the multiplication is integer and the rendered quantity is exact.
    let milli = if res.cpu_request.ends_with('m') { cpu } else { cpu * 1000.0 };
    let total_milli = (milli * max_pods as f64) as u64;
    let mem = parse_gi(&res.memory_request) * max_pods as u64;
    ResourceQuota {
        metadata: ObjectMeta {
            name: Some("owner".to_string()),
            namespace: Some(ns.to_string()),
            labels: Some(labels(owner, kind)),
            // No ownerReference, same rule as `limit_range`: the namespace outlives any one object
            // in it, and a ceiling that vanished with a rewrite is not a ceiling.
            ..Default::default()
        },
        spec: Some(ResourceQuotaSpec {
            hard: Some(BTreeMap::from([
                ("pods".to_string(), Quantity(max_pods.to_string())),
                ("requests.cpu".to_string(), Quantity(format!("{}", total_milli / 1000))),
                ("requests.memory".to_string(), Quantity(format!("{mem}Gi"))),
            ])),
            ..Default::default()
        }),
    }
}

/// Gibibytes out of a `PodResources` memory string ("4Gi", "2730Mi"). Rounds Mi DOWN to whole Gi:
/// a quota that overstated the ceiling would let the twenty-first pod schedule.
fn parse_gi(q: &str) -> u64 {
    if let Some(g) = q.strip_suffix("Gi") {
        g.parse().unwrap_or(0)
    } else if let Some(m) = q.strip_suffix("Mi") {
        m.parse::<u64>().unwrap_or(0) / 1024
    } else {
        0
    }
}
```

In `bins/agent/src/binding.rs`, directly after the `LimitRange` ensure:

```rust
        ensure(
            &Api::<ResourceQuota>::namespaced(ctx.client.clone(), &ns),
            &k8s::resource_quota(
                &ns,
                owner,
                "workspace",
                &crd::PodResources::default(),
                kloudlite_git_workspaces::model::max_per_owner(),
            ),
            ctx,
        )
        .await?;
```

In `deploy/k3s/agent-rbac.yaml`, add `resourcequotas` to the same rule as `limitranges` (`:215`) and add the matching row to the header table beside the `limitranges` line:

```
#   resourcequotas                          create,patch                 ensure (server-side apply)
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p kloudlite-git-workspaces -p kloudlite-git-agent-bin -- --test-threads=1; echo exit=$?` and `cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?`
Expected: exit=0 both.

- [ ] **Step 5: Commit**

```bash
git add crates/workspaces/src/k8s.rs bins/agent/src/binding.rs deploy/k3s/agent-rbac.yaml
git commit -m "Give each owner namespace a ResourceQuota beside its LimitRange"
```

---

### Task 4: Every listing decides on `spec.owner`, never on the label (I1, I2)

**Files:**
- Modify: `crates/workspaces/src/api.rs:741` (`list_ws`), `:796-815` (`ssh_session`'s name fallback), `:379` (`pushed_volumes` fallback), `:1683` (`list_env`), `:2092` (`live_parents`), `:2184` (`list_volumes`)
- Test: `crates/workspaces/tests/api_user.rs`, `crates/workspaces/tests/api_volumes.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `pub trait Owned { fn owner(&self) -> &str; }` implemented for `crd::Workspace`, `crd::Environment`, `crd::Snapshot`, and `fn mine<K: Owned>(items: Vec<K>, owners: &[String]) -> Vec<K>`. Task 10 moves both into `api/scope.rs` unchanged.

- [ ] **Step 1: Write the failing tests**

In `crates/workspaces/tests/api_user.rs`:

```rust
/// A mislabelled object — a restored backup, a migration, an operator with kubectl, or the window
/// before the controller re-stamps — must not become an ssh session into someone else's workspace.
/// `ssh_session` mints a token off whatever this lookup returns, so the name fallback rechecks
/// `spec.owner` exactly as `my_ws` does.
#[tokio::test]
async fn the_ssh_name_fallback_refuses_a_mislabelled_workspace() {
    let mut foreign = placed_ws("ws-bob", "bob");
    // The label says karthik; the spec says bob. The spec is the truth.
    foreign["metadata"]["labels"]["kloudlite-git.io/owner"] = json!("karthik");
    foreign["spec"]["name"] = json!("target");
    foreign["status"]["sshHostKey"] = json!("ssh-ed25519 AAAA");
    let s = server(vec![
        get(format!("{API}/workspaces/target"), json!({"kind": "Status", "apiVersion": "v1", "status": "Failure", "reason": "NotFound", "code": 404})),
        get(format!("{API}/workspaces"), json!({"apiVersion": "kloudlite-git.io/v1alpha1", "kind": "WorkspaceList", "metadata": {}, "items": [foreign]})),
    ])
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/target/ssh-session", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404, "{}", resp.text().await.unwrap());
}

/// Same class, in the listing: a label is an index, never an answer.
#[tokio::test]
async fn list_ws_drops_a_mislabelled_workspace() {
    let mut foreign = placed_ws("ws-bob", "bob");
    foreign["metadata"]["labels"]["kloudlite-git.io/owner"] = json!("karthik");
    let s = server(vec![
        get(format!("{API}/workspaces"), json!({"apiVersion": "kloudlite-git.io/v1alpha1", "kind": "WorkspaceList", "metadata": {}, "items": [foreign, placed_ws("ws-mine", "karthik")]})),
        get(format!("{API}/snapshots"), json!({"apiVersion": "kloudlite-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": []})),
    ])
    .await;
    let resp = reqwest::Client::new()
        .get(format!("{}/v1/workspaces", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let ids: Vec<&str> = body.as_array().unwrap().iter().map(|w| w["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["ws-mine"], "bob's workspace is not karthik's: {body}");
}
```

In `crates/workspaces/tests/api_volumes.rs`:

```rust
/// `list_volumes` derives the row's `vol/{owner}/{name}` from the first snapshot's `spec.owner`,
/// so a mislabelled snapshot both appears in the wrong person's list and mislabels who owns it.
#[tokio::test]
async fn list_volumes_drops_a_mislabelled_snapshot() {
    let mut foreign = push("ws-2-a", "ws-2", "alice", "2026-08-27T09:00:00Z");
    foreign["metadata"]["labels"]["kloudlite-git.io/owner"] = json!("karthik");
    let s = server(vec![
        kget(SNAPS, snap_list(vec![push("ws-1-a", "ws-1", "karthik", "2026-08-27T09:00:00Z"), foreign])),
        kget(format!("{API}/workspaces"), ws_list(vec![])),
        kget(format!("{API}/environments"), env_list(vec![])),
    ])
    .await;
    let (status, body) = get_json(&s, &token(&s.jwt, "karthik"), "/v1/volumes").await;
    assert_eq!(status, 200, "{body}");
    let names: Vec<&str> = body.as_array().unwrap().iter().map(|v| v["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["ws-1"], "alice's volume is not karthik's: {body}");
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p kloudlite-git-workspaces --test api_user --test api_volumes -- --test-threads=1; echo exit=$?`
Expected: FAIL — the ssh test gets 201, both listings show the foreign row.

- [ ] **Step 3: Add `Owned`/`mine` and route every listing through it**

In `crates/workspaces/src/api.rs`, beside `owned_by`:

```rust
/// The `spec.owner` of anything this API lists. One trait so "narrow by label, DECIDE on spec" is
/// a single function instead of a rule seven handlers each remembered or forgot.
pub trait Owned {
    fn owner(&self) -> &str;
}

impl Owned for crd::Workspace {
    fn owner(&self) -> &str {
        &self.spec.owner
    }
}
impl Owned for crd::Environment {
    fn owner(&self) -> &str {
        &self.spec.owner
    }
}
impl Owned for crd::Snapshot {
    fn owner(&self) -> &str {
        &self.spec.owner
    }
}

/// Keep only what `owners` actually owns. The label selector stays as the INDEX; this is the
/// answer. An object whose label disagrees with its spec — a restored backup, a migration, an
/// operator with kubectl, the window before the controller re-stamps — is somebody else's.
pub fn mine<K: Owned>(items: Vec<K>, owners: &[String]) -> Vec<K> {
    items.into_iter().filter(|k| owners.iter().any(|o| o == k.owner())).collect()
}
```

Then:
- `list_ws:741` — `let items = mine(api.list(&owned_in(&owner, &team)).await.map_err(kube_err)?.items, std::slice::from_ref(&owner));`
- `ssh_session:806-812` — insert `.filter(|w| w.spec.owner == owner)` before `.find(|w| w.spec.name == target)`.
- `pushed_volumes:379` — wrap the listed items in `mine(..., std::slice::from_ref(&owner.to_string()))` before the `Ready` filter. (Task 13 deletes this function; the one-line fix stands until then.)
- `list_env:1683` — `for e in mine(api.list(&owned_by(&owner)).await.map_err(kube_err)?.items, std::slice::from_ref(&owner))`
- `live_parents:2097,2103` — `for w in mine(ws.list(&lp).await.ok()?.items, owners)` and the same for environments.
- `list_volumes:2184` — `let snaps = mine(api.list(...).await.map_err(kube_err)?.items, &owners);`

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p kloudlite-git-workspaces -p kloudlite-git-agent-bin -- --test-threads=1; echo exit=$?` and the clippy gate.
Expected: exit=0 both.

- [ ] **Step 5: Commit**

```bash
git add crates/workspaces/src/api.rs crates/workspaces/tests/api_user.rs crates/workspaces/tests/api_volumes.rs
git commit -m "Decide every listing on spec.owner instead of the owner label"
```

---

### Task 5: The running-base check reads every live parent's source, not just `status.head` (I3)

**Files:**
- Modify: `crates/workspaces/src/api.rs:2069-2074` (`struct Parent`), `:2122-2139` (`parents_of_volume`), `:2320-2325` and `:2275` (the two checks)
- Test: `crates/workspaces/tests/api_volumes.rs`

**Interfaces:**
- Consumes: `snapshots_on_volume` from Task 1.
- Produces: `Parent` gains `base: Option<String>` — the snapshot the parent's `spec.storage.source` names (`CloneOf { commit: Some(id) }` or `SeededFrom { snapshot: id }`), `None` otherwise. Task 7 rewrites how `parents_of_volume` FINDS its parents; the struct and the two checks stay as this task leaves them.

- [ ] **Step 1: Write the failing test**

```rust
/// `status.head` is written only by the node running the pod, so a workspace created by a restore
/// has `head == None` from the create until that node's first checkout — minutes on a cold node,
/// indefinitely while it is down. Deleting its base in that window is permanent: `engine::checkout`
/// answers NO_SUCH_RECORD and the volume controller classifies that as permanent, never retried.
#[tokio::test]
async fn a_restore_that_has_not_checked_out_yet_still_protects_its_base() {
    let mut restored = ws_obj("ws-restored", "karthik", "recovered");
    restored["spec"]["storage"]["source"] = json!({"cloneOf": {"volume": "ws-1", "commit": "ws-1-a"}});
    // No status at all: no node has claimed it yet, so there is no head and no volumeRef.
    restored["status"] = Value::Null;
    let s = server(vec![
        kget(SNAPS, snap_list(vec![push("ws-1-a", "ws-1", "karthik", "2026-08-27T09:00:00Z")])),
        kget(format!("{API}/workspaces"), ws_list(vec![restored])),
        kget(format!("{API}/environments"), env_list(vec![])),
    ])
    .await;
    let tok = token(&s.jwt, "karthik");

    assert_eq!(delete(&s, &tok, "/v1/volumes/ws-1/snapshots/ws-1-a").await, 409, "an unplaced restore's base");
    assert_eq!(delete(&s, &tok, "/v1/volumes/ws-1").await, 409, "and the volume under it");
    assert!(s.rec.calls().iter().all(|c| !c.starts_with("DELETE")), "nothing was deleted: {:?}", s.rec.calls());
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p kloudlite-git-workspaces --test api_volumes a_restore_that_has_not_checked_out -- --test-threads=1; echo exit=$?`
Expected: FAIL with 204 on the snapshot delete.

- [ ] **Step 3: Carry the source snapshot on `Parent`**

```rust
/// A live Workspace/Environment, reduced to what the volume routes need of it.
struct Parent {
    kind: String,
    display: String,
    /// `status.head` — the snapshot it is standing on, which `delete_snapshot` refuses to remove.
    head: Option<String>,
    /// The snapshot its SPEC was grafted onto. `head` only exists once the owning node has checked
    /// out; between the create and that first checkout the spec is the only record that this
    /// snapshot is load-bearing, and deleting it there is unrecoverable.
    base: Option<String>,
}

/// The snapshot a parent's volume source names, if any.
fn source_snapshot(storage: &Option<crd::WorkspaceStorage>) -> Option<String> {
    match storage.as_ref()?.source.as_ref()? {
        VolumeSource::CloneOf { commit, .. } => commit.clone(),
        VolumeSource::SeededFrom { snapshot, .. } => Some(snapshot.clone()),
        _ => None,
    }
}
```

Fill `base` at all four `Parent` construction sites (`live_parents` twice, `parents_of_volume` twice) with `source_snapshot(&w.spec.storage)` / `source_snapshot(&e.spec.storage)`.

In `parents_of_volume`'s matcher, a parent that names this volume as its SOURCE is on this volume even before it reports a `volumeRef`:

```rust
    let on_volume = |vref: Option<String>, name: String, storage: &Option<crd::WorkspaceStorage>| {
        // Three ways to be on this volume: the node said so, the parent IS the volume (an owned
        // one shares its id), or its spec grafts onto it and no node has answered yet.
        vref.clone().unwrap_or(name) == volume
            || matches!(
                storage.as_ref().and_then(|s| s.source.as_ref()),
                Some(VolumeSource::CloneOf { volume: v, .. } | VolumeSource::SeededFrom { volume: v, .. }) if v == volume
            )
    };
```

In `delete_snapshot`'s refusal (`:2323`) and in `delete_volume`'s emptiness check, consult both fields:

```rust
    if live.iter().any(|p| {
        p.head.as_deref() == Some(snapshot.as_str()) || p.base.as_deref() == Some(snapshot.as_str())
    }) {
        return Err((StatusCode::CONFLICT, "this snapshot is the base of a running worktree").into_response());
    }
```

`delete_volume` already refuses on any non-empty `parents_of_volume`, so the widened matcher covers it — no second clause needed there.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p kloudlite-git-workspaces -p kloudlite-git-agent-bin -- --test-threads=1; echo exit=$?` and the clippy gate.
Expected: exit=0 both.

- [ ] **Step 5: Commit**

```bash
git add crates/workspaces/src/api.rs crates/workspaces/tests/api_volumes.rs
git commit -m "Protect a snapshot a live parent's spec is grafted onto"
```

---

### Task 6: `/v1/regions` returns a projection, and the dead Azure fields go (I4, CL7)

**Files:**
- Modify: `crates/workspaces/src/model.rs:8-15` (`Region`)
- Modify: `crates/workspaces/src/api.rs:236-281` (`NewRegion`, `create_region`, `list_regions`)
- Modify: `crates/workspaces/tests/meta_store.rs`, `crates/workspaces/tests/api_user.rs:192` (the `region()` helper)
- Test: `crates/workspaces/tests/api_user.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `model::Region { id, name, status }` — the two Azure fields are gone. Task 9 replaces the type entirely with `crd::Region` + a `RegionDoc`; this task is the field deletion only.

- [ ] **Step 1: Write the failing test**

```rust
/// Snapshot bytes have had no object store since the durable-snapshots cutover, so the storage
/// account and container are dead weight — and publishing them to every signed-in caller is a free
/// map of our infrastructure.
#[tokio::test]
async fn listing_regions_never_names_a_storage_account() {
    let s = server(vec![]).await;
    let resp = reqwest::Client::new()
        .get(format!("{}/v1/regions", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let row = &body.as_array().unwrap()[0];
    assert_eq!(row["id"], "centralindia");
    assert!(row.get("storage_account").is_none(), "no infrastructure topology: {body}");
    assert!(row.get("blob_container").is_none(), "no infrastructure topology: {body}");
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p kloudlite-git-workspaces --test api_user listing_regions_never_names -- --test-threads=1; echo exit=$?`
Expected: FAIL — both keys present.

- [ ] **Step 3: Delete the fields**

`crates/workspaces/src/model.rs`:

```rust
/// A region a workspace may run in. `status` is `active` or `inactive`: re-registering is the only
/// way to retire one, and a retired region must stop being offered while its records stay readable.
///
/// No storage account and no blob container: snapshot bytes are btrfs subvolumes replicated
/// between agents and have never touched an object store since the durable-snapshots cutover.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Region {
    pub id: String,
    pub name: String,
    pub status: String,
}
```

Drop `storage_account`/`blob_container` from `NewRegion` and from `create_region`'s construction; delete them from `tests/meta_store.rs`'s `region()` fixture and its round-trip assertion, and from `tests/api_user.rs`'s `region()` helper.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p kloudlite-git-workspaces -p kloudlite-git-agent-bin -- --test-threads=1; echo exit=$?` and the clippy gate.
Expected: exit=0 both.

- [ ] **Step 5: Commit**

```bash
git add crates/workspaces/src/api.rs crates/workspaces/src/model.rs crates/workspaces/tests/
git commit -m "Stop publishing storage account names on /v1/regions"
```

---

### Task 7: A selectable `status.volumeRef`, and the deletes stop scanning the cluster (I5)

**Files:**
- Modify: `crates/workspaces/src/crd.rs:558-570` (`Workspace`'s `#[kube]`), `:683-695` (`Environment`'s)
- Modify: `crates/workspaces/src/api.rs:2092-2139` (`live_parents`, `parents_of_volume`)
- Modify: `deploy/k3s/crds.yaml` (regenerated), `crates/workspaces/tests/crd_yaml.rs:47-53` (the expected selector sets)
- Test: `crates/workspaces/tests/api_volumes.rs`, `crates/workspaces/tests/crd_yaml.rs`

**Interfaces:**
- Consumes: `Parent { kind, display, head, base }` and `source_snapshot` from Task 5.
- Produces: no signature change — `parents_of_volume` keeps `async fn (…) -> Option<Vec<Parent>>`. Only how it queries changes.

**The two selectors, and why both:** `status.volumeRef={volume}` finds every PLACED parent. A parent that no node has claimed yet has an empty `volumeRef`, which the API server indexes as the empty string — so `status.volumeRef=` is exactly the set of unplaced parents, bounded by how many creates are in flight, and their `spec.storage.source` is read locally. Four indexed lists replace two full-cluster scans, and the unplaced set is what makes Task 5's protection survive the change.

**Deploy caveat, to repeat in the commit message:** `selectableFields` is a schema change. `deploy/k3s/crds.yaml` must be applied before the api image that queries on it rolls, or every such list answers 400.

- [ ] **Step 1: Write the failing tests**

In `crates/workspaces/tests/crd_yaml.rs`, extend the expected set:

```rust
            "Workspace" | "Environment" => &[".status.nodeName", ".status.volumeRef"],
```

In `crates/workspaces/tests/api_volumes.rs`:

```rust
/// Two unfiltered cluster-wide LISTs per delete — four for a snapshot delete, which re-reads on
/// purpose — deserialized every Workspace and Environment in the cluster to answer one question.
/// The selectable field makes both reads indexed; the empty-value selector is what keeps an
/// unplaced parent visible.
#[tokio::test]
async fn the_delete_paths_select_on_the_volume_ref() {
    let s = server(vec![
        kget(SNAPS, snap_list(vec![push("ws-1-a", "ws-1", "karthik", "2026-08-27T09:00:00Z")])),
        kget(format!("{API}/workspaces"), ws_list(vec![])),
        kget(format!("{API}/environments"), env_list(vec![])),
        ok("DELETE", format!("{SNAPS}/ws-1-a")),
        ok("DELETE", format!("{API}/volumes/ws-1")),
    ])
    .await;
    assert_eq!(delete(&s, &token(&s.jwt, "karthik"), "/v1/volumes/ws-1/snapshots/ws-1-a").await, 204);

    let listed: Vec<String> = s
        .rec
        .requests()
        .into_iter()
        .filter(|r| r.contains("/workspaces?") || r.contains("/environments?"))
        .collect();
    assert!(!listed.is_empty(), "the parents are still consulted: {listed:?}");
    assert!(
        listed.iter().all(|r| r.contains("fieldSelector=status.volumeRef")),
        "every parent read is indexed: {listed:?}"
    );
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p kloudlite-git-workspaces --test crd_yaml --test api_volumes -- --test-threads=1; echo exit=$?`
Expected: FAIL — the selector set does not match, and the listing carries no `fieldSelector`.

- [ ] **Step 3: Declare the field and query on it**

In `crd.rs`, add to both `#[kube(...)]` blocks beside the existing `selectable = ".status.nodeName"`:

```rust
    // `parents_of_volume` asks "what is running on this volume" on every snapshot and volume
    // delete; without this it was two full-cluster lists per question. An unset value indexes as
    // the empty string, which is what makes "not placed yet" its own queryable set.
    selectable = ".status.volumeRef",
```

Rewrite `parents_of_volume`:

```rust
async fn parents_of_volume(s: &ApiState, volume: &str) -> Option<Vec<Parent>> {
    let c = s.kube.as_ref()?;
    // Placed parents come back by the indexed field. Unplaced ones — created seconds ago, or
    // waiting on a node that is down — have no `volumeRef` at all, and the API server indexes that
    // as the empty string: a small, bounded set whose `spec.storage.source` says what they graft
    // onto. Both, because Task 5's protection depends on the second.
    let placed = ListParams::default().fields(&format!("status.volumeRef={volume}"));
    let unplaced = ListParams::default().fields("status.volumeRef=");
    let mut out = vec![];
    for lp in [&placed, &unplaced] {
        for w in Api::<crd::Workspace>::all(c.clone()).list(lp).await.ok()?.items {
            let st = w.status.as_ref();
            let base = source_snapshot(&w.spec.storage);
            if on_volume(volume, st.and_then(|s| s.volume_ref.clone()), w.name_any(), &w.spec.storage) {
                out.push(Parent {
                    kind: "workspace".into(),
                    display: w.spec.name.clone(),
                    head: st.and_then(|s| s.head.clone()),
                    base,
                });
            }
        }
        for e in Api::<crd::Environment>::all(c.clone()).list(lp).await.ok()?.items {
            let st = e.status.as_ref();
            let base = source_snapshot(&e.spec.storage);
            if on_volume(volume, st.and_then(|s| s.volume_ref.clone()), e.name_any(), &e.spec.storage) {
                out.push(Parent {
                    kind: "environment".into(),
                    display: e.spec.name.clone(),
                    head: st.and_then(|s| s.head.clone()),
                    base,
                });
            }
        }
    }
    Some(out)
}
```

with Task 5's closure lifted to a free function so both loops share it:

```rust
/// On this volume: the node said so, the parent IS the volume (an owned one shares its id), or its
/// spec grafts onto it and no node has answered yet.
fn on_volume(volume: &str, vref: Option<String>, name: String, storage: &Option<crd::WorkspaceStorage>) -> bool {
    vref.unwrap_or(name) == volume
        || matches!(
            storage.as_ref().and_then(|s| s.source.as_ref()),
            Some(VolumeSource::CloneOf { volume: v, .. } | VolumeSource::SeededFrom { volume: v, .. }) if v == volume
        )
}
```

`live_parents` keeps its label selector (it is owner-scoped by design) — leave it alone.

- [ ] **Step 4: Regenerate the manifest and run the tests**

Run:
```
CRD_REGEN=1 cargo test -p kloudlite-git-workspaces --test crd_yaml; echo exit=$?
cargo test -p kloudlite-git-workspaces -p kloudlite-git-agent-bin -- --test-threads=1; echo exit=$?
cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?
```
Expected: exit=0 all three, with `deploy/k3s/crds.yaml` modified.

- [ ] **Step 5: Commit**

```bash
git add crates/workspaces/src/crd.rs crates/workspaces/src/api.rs crates/workspaces/tests/ deploy/k3s/crds.yaml
git commit -m "Select parents on status.volumeRef instead of scanning the cluster

Apply deploy/k3s/crds.yaml before rolling the api image: the field selector
is a schema declaration, and a query on an undeclared field is a 400."
```

---

### Task 8: A restore refuses a snapshot cut from the other kind (I6)

**Files:**
- Modify: `crates/workspaces/src/api.rs:1345-1361` (`restore_ws`), `:1590-1607` (`restore_env`)
- Test: `crates/workspaces/tests/api_user.rs`

**Interfaces:**
- Consumes: `find_commit_model_snapshot_for_restore` (unchanged).
- Produces: nothing new.

- [ ] **Step 1: Write the failing tests**

```rust
/// Restoring an environment snapshot into a workspace used to fall through `_ => None` and produce
/// a workspace with the DEFAULT image, default quota and no packages, mounting a database's data
/// directory. Not an escalation — `caller_owners` gates readability either way — but a request the
/// API should refuse rather than half-honour.
#[tokio::test]
async fn a_workspace_restore_refuses_an_environment_snapshot() {
    let snap = json!({
        "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "Snapshot",
        "metadata": {"name": "env-1-a", "labels": {"kloudlite-git.io/owner": "karthik"}},
        "spec": {"volume": "env-1", "owner": "karthik", "worktree": "env-1", "parent": "",
                 "state": {"kind": "environment", "services": [], "quotaGb": 20}},
        "status": {"phase": "ready", "readyAt": "2026-08-27T09:00:00Z"}
    });
    let s = server(vec![get(format!("{API}/snapshots/env-1-a"), snap)]).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/restore", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"name": "wrong-kind", "snapshot_id": "env-1-a"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body = resp.text().await.unwrap();
    assert!(body.contains("/v1/environments/restore"), "the answer names the right route: {body}");
    assert!(s.rec.sent("POST", &format!("{API}/workspaces")).is_empty(), "nothing written");
}

/// The twin. A `state: None` legacy snapshot keeps today's behaviour in both — "absent means old".
#[tokio::test]
async fn an_environment_restore_refuses_a_workspace_snapshot() {
    let snap = json!({
        "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "Snapshot",
        "metadata": {"name": "ws-1-a", "labels": {"kloudlite-git.io/owner": "karthik"}},
        "spec": {"volume": "ws-1", "owner": "karthik", "worktree": "ws-1", "parent": "",
                 "state": {"kind": "workspace", "image": "alpine:3.20", "packages": [],
                           "resources": {}, "quotaGb": 20}},
        "status": {"phase": "ready", "readyAt": "2026-08-27T09:00:00Z"}
    });
    let s = server(vec![get(format!("{API}/snapshots/ws-1-a"), snap)]).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/environments/restore", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"name": "wrong-kind", "snapshot_id": "ws-1-a"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert!(s.rec.sent("POST", &format!("{API}/environments")).is_empty(), "nothing written");
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p kloudlite-git-workspaces --test api_user restore_refuses -- --test-threads=1; echo exit=$?`
Expected: FAIL — both get 202 (or a later 4xx for the wrong reason).

- [ ] **Step 3: Refuse the mismatch**

In `restore_ws`, replace the `frozen` match's `_ => None` arm so a present-but-wrong-kind state is a refusal rather than a silent default:

```rust
    // A `state` from the other kind is a request to refuse, not to half-honour: restoring an
    // environment snapshot as a workspace mounts a database's data directory under the default
    // image with no packages. `None` is a snapshot cut before states existed — "absent means old",
    // and every reader keeps its fallback for it.
    let frozen = match &snap.spec.state {
        Some(crd::SnapshotState::Workspace { image, packages, resources, quota_gb, attached_environment }) => {
            Some((image.clone(), packages.clone(), resources.clone(), *quota_gb, attached_environment.clone()))
        }
        Some(crd::SnapshotState::Environment { .. }) => {
            return Err((
                StatusCode::BAD_REQUEST,
                "this snapshot was cut from an environment; use POST /v1/environments/restore",
            )
                .into_response())
        }
        None => None,
    };
```

The mirror in `restore_env`:

```rust
        Some(crd::SnapshotState::Workspace { .. }) => {
            return Err((
                StatusCode::BAD_REQUEST,
                "this snapshot was cut from a workspace; use POST /v1/workspaces/restore",
            )
                .into_response())
        }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p kloudlite-git-workspaces -p kloudlite-git-agent-bin -- --test-threads=1; echo exit=$?` and the clippy gate.
Expected: exit=0 both.

- [ ] **Step 5: Commit**

```bash
git add crates/workspaces/src/api.rs crates/workspaces/tests/api_user.rs
git commit -m "Refuse a restore of a snapshot cut from the other kind"
```

---

### Task 9: `Region` becomes a CRD, and the Cosmos tier goes (audit cut a)

**Files:**
- Modify: `crates/workspaces/src/crd.rs` (new `Region`/`RegionSpec`, added to `all_crds`)
- Modify: `crates/workspaces/src/api.rs:22,83-138,227-281,522-528` (`ApiState`, `store_err`, the region routes, `check_region`)
- Modify: `crates/workspaces/src/lib.rs` (drop `pub mod cosmos; pub mod store;`)
- Modify: `crates/workspaces/src/model.rs` (drop `Region`)
- Modify: `crates/workspaces/Cargo.toml` (drop `azure_data_cosmos`, `azure_core`, `reqwest012`, `futures` if it becomes unused), root `Cargo.toml` (drop the `azure_data_cosmos` workspace dep and its comment block)
- Modify: `bins/api/src/main.rs:106-125`
- Modify: `deploy/k3s/api-rbac.yaml`, `deploy/k3s/README.md`, `deploy/kloudlite-git.yaml:132-136,428-432`, `tests/ws_e2e.sh:69,105-108,146-151,222-256`, `.local/run-api.sh`, `CLAUDE.md`
- Modify: every test `server*()` helper: `crates/workspaces/tests/api_user.rs:87-155`, `api_volumes.rs:74-83`, `api_teams.rs:69`, `api_commit_model.rs:91`
- Delete: `crates/workspaces/src/cosmos.rs`, `crates/workspaces/src/store.rs`, `crates/workspaces/tests/meta_store.rs`
- Test: `crates/workspaces/tests/api_user.rs`, `crates/workspaces/tests/crd_yaml.rs`

**Interfaces:**
- Consumes: Task 6's three-field `Region` shape.
- Produces:
  ```rust
  // crd.rs
  #[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
  #[kube(group = "kloudlite-git.io", version = "v1alpha1", kind = "Region", plural = "regions",
         status = "RegionStatus", printcolumn = ...)]
  #[serde(rename_all = "camelCase")]
  pub struct RegionSpec { pub name: String, pub status: String }
  #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
  pub struct RegionStatus {}
  ```
  ```rust
  // api.rs
  pub struct ApiState { pub jwt: Arc<Jwt>, pub admins: HashSet<String>, /* …no `store`… */ }
  impl ApiState { pub fn new(jwt: Arc<Jwt>, admins: HashSet<String>) -> Self }
  ```
  `list_regions` answers `[{"id","name","status"}]` — the same three keys Task 6 left, now built from `crd::Region` by a private `RegionDoc`.

**Why a CRD and not a ConfigMap:** every other object in this control plane is a CRD, the api tier already holds a `kube::Client`, and a CRD gets the same RBAC, the same `kubectl get`, and the same generated schema. It also removes the "restart forgets the region" trap the `MemStore` dev fallback carried. `RegionStatus` is empty and exists only so the kind has a `/status` subresource like every sibling (`every_crd_has_a_status_subresource_and_the_right_node_selector` asserts it); `spec.status` stays the active/inactive flag because `/v1` writes it and it is desired state, not an observation.

- [ ] **Step 1: Write the failing tests**

In `crates/workspaces/tests/crd_yaml.rs`, add `"Region" => &[],` to the selector match and expect it in `all_crds`.

In `crates/workspaces/tests/api_user.rs`:

```rust
/// Regions are a CRD like everything else in this control plane: an admin POST writes one object,
/// and a create reads it back by name. No Cosmos, and no in-memory fallback that a restart forgets.
#[tokio::test]
async fn registering_a_region_writes_one_custom_resource() {
    let s = server_with(
        &["admin@example.com"],
        Some(vec![post(format!("{API}/regions"), region_obj("centralindia"))]),
    )
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/regions", s.base))
        .bearer_auth(s.jwt.mint("admin@example.com", "Admin", Some("admin")).unwrap())
        .json(&json!({"id": "centralindia", "name": "Central India"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "{}", resp.text().await.unwrap());
    let r = &s.rec.sent("POST", &format!("{API}/regions"))[0];
    assert_eq!(r["metadata"]["name"], "centralindia");
    assert_eq!(r["spec"]["name"], "Central India");
    assert_eq!(r["spec"]["status"], "active", "a new region is active unless it says otherwise");
}

/// An inactive region must stop being offered to new workspaces while its existing records stay
/// readable — the one rule the region routes have ever had.
#[tokio::test]
async fn creating_in_an_inactive_region_is_refused() {
    let mut inactive = region_obj("centralindia");
    inactive["spec"]["status"] = json!("inactive");
    let s = server(vec![get(format!("{API}/regions/centralindia"), inactive)]).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"name": "web", "region": "centralindia", "quota_gb": 20}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 422, "{}", resp.text().await.unwrap());
}
```

with the fixture:

```rust
fn region_obj(id: &str) -> Value {
    json!({
        "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "Region",
        "metadata": {"name": id},
        "spec": {"name": id, "status": "active"}
    })
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p kloudlite-git-workspaces --test api_user --test crd_yaml -- --test-threads=1; echo exit=$?`
Expected: FAIL to compile (`crd::Region` does not exist).

- [ ] **Step 3: Add the CRD**

In `crd.rs`, beside the other kinds:

```rust
/// A region a workspace may run in — cluster-scoped, like every other kind here.
///
/// Cross-cluster metadata by nature, and it used to live in Cosmos for exactly that reason. It does
/// not need to: a region is registered by an admin, read on every create, and changed almost never,
/// so the cheapest correct home is the API server this tier already talks to. `spec.status` is
/// DESIRED state (`active`/`inactive`) — re-registering is the only way to retire one, and a
/// retired region stops being offered while its existing workspaces keep running.
#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "kloudlite-git.io",
    version = "v1alpha1",
    kind = "Region",
    plural = "regions",
    status = "RegionStatus",
    printcolumn = r#"{"name":"Status","type":"string","jsonPath":".spec.status"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct RegionSpec {
    /// What a person sees in the region picker. The object's NAME is the id.
    pub name: String,
    /// `active` or `inactive`.
    pub status: String,
}

/// Empty on purpose: no controller observes a region. It exists so the kind has the same
/// `/status` subresource split every sibling has, rather than being the one kind where a status
/// write would fold into spec.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RegionStatus {}
```

Add `Region::crd()` to `all_crds()`.

- [ ] **Step 4: Rewrite the region routes and `ApiState`**

In `api.rs`: delete `use crate::store::MetaStore;`, the `store` field, `store_err`, and the `MetaStore` parameter of `ApiState::new`. Then:

```rust
#[derive(serde::Deserialize)]
struct NewRegion {
    id: String,
    name: String,
    /// `active` or `inactive`. Re-registering a region is the only way to retire one — there is no
    /// delete — and a retired region must stop being offered to new workspaces while its existing
    /// records stay readable.
    #[serde(default = "active_status")]
    status: String,
}

/// What a caller sees: the three fields `check_region` and the web consume, and nothing about where
/// the region's infrastructure lives.
#[derive(serde::Serialize)]
struct RegionDoc {
    id: String,
    name: String,
    status: String,
}

fn region_doc(r: &crd::Region) -> RegionDoc {
    RegionDoc { id: r.name_any(), name: r.spec.name.clone(), status: r.spec.status.clone() }
}

async fn create_region(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<NewRegion>,
) -> Result<Response, Response> {
    // Admin gating keys on the EMAIL (the allowlist's identity), not the username `caller`
    // resolves — an admin needs no username to register regions.
    let tok = bearer_token(&headers).ok_or_else(unauthorized)?;
    let email = s.jwt.verify(tok.trim()).map(|c| c.sub).map_err(|_| unauthorized())?;
    require_admin(&s, &email)?;
    // The id becomes an object name and a gateway hostname label, so it goes through the same
    // segment check every other path segment here does.
    check_path_segment(&body.id)?;
    let status = if body.status == "inactive" { "inactive" } else { "active" };
    let r = crd::Region::new(&body.id, crd::RegionSpec { name: body.name, status: status.into() });
    // Apply, not create: re-registering IS how a region is retired or renamed, so a second POST of
    // the same id must not be a 409.
    let api: Api<crd::Region> = Api::all(kube(&s)?.clone());
    let saved = api
        .patch(&body.id, &PatchParams::apply("kloudlite-git-api").force(), &Patch::Apply(&r))
        .await
        .map_err(kube_err)?;
    Ok((StatusCode::CREATED, Json(region_doc(&saved))).into_response())
}

async fn list_regions(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
) -> Result<Response, Response> {
    caller(&s, &headers).await?;
    let api: Api<crd::Region> = Api::all(kube(&s)?.clone());
    let rows: Vec<RegionDoc> =
        api.list(&ListParams::default()).await.map_err(kube_err)?.items.iter().map(region_doc).collect();
    Ok(Json(rows).into_response())
}
```

and `check_region` becomes a point read:

```rust
/// A region is an id the caller typed, and it becomes the OwnerBinding's name and the gateway
/// hostname. Unknown: a workspace no controller ever claims. Chosen: a binding name squatted in
/// someone else's region. Only what an admin registered and left active gets through.
async fn check_region(s: &ApiState, region: &str) -> Result<(), Response> {
    check_path_segment(region)?;
    let api: Api<crd::Region> = Api::all(kube(s)?.clone());
    let active = api
        .get_opt(region)
        .await
        .map_err(kube_err)?
        .is_some_and(|r| r.spec.status == "active");
    if active {
        return Ok(());
    }
    Err((StatusCode::UNPROCESSABLE_ENTITY, Json(serde_json::json!({"error": "unknown region"}))).into_response())
}
```

Delete `crates/workspaces/src/cosmos.rs`, `crates/workspaces/src/store.rs`, `crates/workspaces/tests/meta_store.rs`, their `pub mod` lines in `lib.rs`, `model::Region`, and the `store_err` case in `backend_error_text_never_reaches_the_caller` (`api.rs:2484`).

- [ ] **Step 5: Rewire `bins/api`, the deps and the deploy surface**

`bins/api/src/main.rs`: drop the whole `meta_store` block and the `COSMOS_ENDPOINT` warning; `ApiState::new(jwt, admins)`. Fix the module doc's "talks to Cosmos" line to say it reads through a cache and the cluster.

`crates/workspaces/Cargo.toml`: delete `azure_data_cosmos`, `azure_core`, `reqwest012` and their comment blocks. Root `Cargo.toml`: delete the `azure_data_cosmos` workspace dependency and the paragraph above it that explains the second reqwest major.

`deploy/k3s/api-rbac.yaml`: add to the ClusterRole, with the reason:

```yaml
  # `/v1/regions` is the only writer. `patch` because re-registering a region IS how one is
  # retired — there is no delete — and a create would 409 on the second POST of the same id.
  - apiGroups: ["kloudlite-git.io"]
    resources: ["regions"]
    verbs: ["get", "list", "create", "patch"]
```

`deploy/k3s/README.md`: note in the apply order that `crds.yaml` now carries `Region`, and that regions are registered with `POST /v1/regions` rather than being seeded into Cosmos.

`deploy/kloudlite-git.yaml`: delete both `COSMOS_ENDPOINT`/`COSMOS_KEY`/`COSMOS_DB` env blocks (`:132-136`, `:428-432`).

`tests/ws_e2e.sh`: delete the `COSMOS_ENDPOINT`/`COSMOS_KEY` precondition at `:69` and its `exit 77`, the `WS_E2E_COSMOS_DB` block at `:105-108`, the `az cosmosdb sql database delete` cleanup at `:146-151`, and the three `COSMOS_*` exports at `:232-234` and `:254-256`; change the two log lines to name the region CR instead of the Cosmos db, and register the test region with a `POST /v1/regions` as an admin before the first create.

`.local/run-api.sh`: no change is needed for regions — it never set `COSMOS_*`. Verify the rest of what it needs is still true: `KLOUDLITE_GIT_MONGO_URI` (directory), `KLOUDLITE_GIT_JWT_SECRET`, `KLOUDLITE_GIT_S3_URL=mem://`, `KLOUDLITE_GIT_PEER_SECRET`, `KUBECONFIG=.local/k3s.yaml`. Add one line to its header comment: regions now live in the k3s cluster the `KUBECONFIG` points at, so a local API sees whatever regions that cluster holds.

`CLAUDE.md`: rewrite the Cosmos sentence in "Workspaces and environments" to say `Region` is a cluster-scoped CRD written by `/v1/regions`, and drop `COSMOS_*` from the `ws_e2e.sh` prerequisites line.

Update every test `server*()` helper to `ApiState::new(jwt.clone(), admins)`, delete the `region(&store, …)` seeding and the `store` field on `Server`, and add `get(format!("{API}/regions/centralindia"), region_obj("centralindia"))` to the route list of every test that creates or restores.

- [ ] **Step 6: Regenerate the manifest and run the tests**

Run:
```
CRD_REGEN=1 cargo test -p kloudlite-git-workspaces --test crd_yaml; echo exit=$?
cargo test -p kloudlite-git-workspaces -p kloudlite-git-agent-bin -- --test-threads=1; echo exit=$?
cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?
cargo tree -p kloudlite-git-workspaces | grep -c azure; echo "azure deps above must be 0"
```
Expected: exit=0 on the first three, `0` azure lines.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "Make Region a CRD and delete the Cosmos tier

Apply deploy/k3s/crds.yaml and deploy/k3s/api-rbac.yaml before rolling the api
image, then re-register each region with POST /v1/regions."
```

---

### Task 10: Split `api.rs` into one module per resource (I7)

**Files:**
- Delete: `crates/workspaces/src/api.rs`
- Create: `crates/workspaces/src/api/mod.rs`, `api/scope.rs`, `api/workspaces.rs`, `api/environments.rs`, `api/volumes.rs`, `api/push.rs`
- Test: `crates/workspaces/tests/api_user.rs`

**Interfaces:**
- Consumes: `Owned`/`mine` (Task 4), `Parent`/`source_snapshot`/`on_volume` (Tasks 5, 7), `RegionDoc`/`ApiState::new(jwt, admins)` (Task 9).
- Produces: no public API change. `kloudlite_git_workspaces::api::{router, ApiState, Directory, OwnerMaterial, refresh_user_keys, owner_set_selector, ATTACHED_ENV_LABEL, KIND_LABEL, OWNER_LABEL, TEAM_LABEL}` all still resolve — `mod.rs` re-exports whatever moved.

**Rule for this task: no behaviour change.** Move code, adjust visibility to `pub(crate)`, add module docs. If a move tempts you to fix something, stop and leave it — a later task or a later review takes it.

- [ ] **Step 1: Write the failing test**

In `crates/workspaces/tests/api_user.rs`:

```rust
/// A split that drops a route is invisible until someone clicks. Every `/v1` path answers 401
/// without a token — an unmounted one answers 404, which is what this catches.
#[tokio::test]
async fn every_v1_route_is_still_mounted() {
    let s = server(vec![]).await;
    let c = reqwest::Client::new();
    let routes: &[(&str, &str)] = &[
        ("POST", "/v1/regions"), ("GET", "/v1/regions"),
        ("POST", "/v1/workspaces"), ("GET", "/v1/workspaces"),
        ("POST", "/v1/workspaces/restore"),
        ("GET", "/v1/workspaces/ws-1"), ("DELETE", "/v1/workspaces/ws-1"), ("PATCH", "/v1/workspaces/ws-1"),
        ("POST", "/v1/workspaces/ws-1/clone"), ("POST", "/v1/workspaces/ws-1/push"),
        ("POST", "/v1/workspaces/ws-1/start"), ("POST", "/v1/workspaces/ws-1/stop"),
        ("POST", "/v1/workspaces/ws-1/attach"), ("POST", "/v1/workspaces/ws-1/detach"),
        ("POST", "/v1/workspaces/ws-1/ssh-session"),
        ("POST", "/v1/environments"), ("GET", "/v1/environments"),
        ("POST", "/v1/environments/restore"),
        ("GET", "/v1/environments/env-1"), ("DELETE", "/v1/environments/env-1"),
        ("POST", "/v1/environments/env-1/start"), ("POST", "/v1/environments/env-1/stop"),
        ("POST", "/v1/environments/env-1/clone"), ("POST", "/v1/environments/env-1/push"),
        ("POST", "/v1/environments/env-1/restore-in-place"),
        ("GET", "/v1/volumes"), ("DELETE", "/v1/volumes/ws-1"),
        ("GET", "/v1/volumes/ws-1/history"), ("GET", "/v1/volumes/ws-1/refs"),
        ("DELETE", "/v1/volumes/ws-1/snapshots/ws-1-a"),
    ];
    for (m, p) in routes {
        let resp = c
            .request(reqwest::Method::from_bytes(m.as_bytes()).unwrap(), format!("{}{p}", s.base))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "{m} {p} is not mounted");
    }
}
```

- [ ] **Step 2: Run it to verify it passes before the split**

Run: `cargo test -p kloudlite-git-workspaces --test api_user every_v1_route_is_still_mounted -- --test-threads=1; echo exit=$?`
Expected: PASS. This one is a *characterization* test: it must be green before the move so its failure after the move means the move broke something. Commit it on its own first.

```bash
git add crates/workspaces/tests/api_user.rs
git commit -m "Assert every /v1 route is mounted before splitting the api module"
```

- [ ] **Step 3: Move the code**

Create `crates/workspaces/src/api/mod.rs` with the existing module doc (corrected: `Region` is a CRD now, not Cosmos), `ApiState`, `Directory`, `OwnerMaterial`, `router()`, `caller`, `unauthorized`, `require_admin`, `kube`, `is_missing`, `kube_err`, `not_found`, `not_ready`, `kube_unavailable`, `check_path_segment`, `rid`, `phase`, the region routes and `check_region`, plus:

```rust
mod environments;
mod push;
mod scope;
mod volumes;
mod workspaces;

// The crate's public surface is unchanged by the split: `bins/api` and the tests name
// `api::{router, ApiState, Directory, …}` and must keep doing so.
pub use scope::{owner_set_selector, Owned};
pub use workspaces::refresh_user_keys;
```

Then move, verbatim: `scope.rs` ← `teams_for`, `may_act_on`, `caller_owners`, `owned_by`, `owned_in`, `owner_set_selector`, `Owned`, `mine`, `my_ws`, `find_env`, `resolve_new_owner`, `owners_namespaces`, `refuse_taken_name`, `refuse_over_cap`; `workspaces.rs` ← every `*_ws` handler plus `ws_doc`, `create_workspace`, `install_user_key_after_placed`, `write_user_key`, `refresh_user_keys`, `check_ws_name`, `clamp_quota`, `bad_packages`, `storage_quota`, `gateway_url`, `drop_attach_policy`, `node_dead_warning`, `interrupted`, `interrupted_409`, `set_desired`; `environments.rs` ← every `*_env` handler plus `env_doc`, `create_environment`, `check_services`, `default_env_quota`; `volumes.rs` ← `list_volumes`, `volume_history`, `volume_refs`, `delete_volume`, `delete_volume_cr`, `delete_snapshot`, `snapshot_rows`, `VolumeSummary`, `Parent`, `live_parents`, `parents_of_volume`, `on_volume`, `source_snapshot`, `kind_of`, `volume_region`, `snapshots_on_volume`, `commit_model_snapshots*`, `find_commit_model_snapshot*`; `push.rs` ← `push_ws`, `push_env`, `create_snapshot`, `optional_push_message`, `PushBody`, `clone_base`, `newest_transient`, `replicated_transients`, `refuse_cut_in_flight`, `BasedOn`, `with_based_on`, `age_seconds`.

Move each existing `#[cfg(test)] mod tests` case to the module that now owns the function it tests.

Head `scope.rs` with the rule the split exists for:

```rust
//! Who the caller is, and what they may act on.
//!
//! The load-bearing part of this module is `mine`: a label selector is an INDEX and `spec.owner` is
//! the answer, and three handlers used to get that right while four got it wrong. One function
//! means the rule cannot be half-remembered. `snapshots_on_volume` in `volumes.rs` is the
//! deliberate exception and says so — a decision that destroys data counts everyone's rows.
```

- [ ] **Step 4: Run the tests to verify nothing moved but the code**

Run: `cargo test -p kloudlite-git-workspaces -p kloudlite-git-agent-bin -- --test-threads=1; echo exit=$?` and the clippy gate.
Expected: exit=0 both, `every_v1_route_is_still_mounted` green, and `git diff --stat` shows no line count change beyond the new module headers.

- [ ] **Step 5: Commit**

```bash
git add -A crates/workspaces/src/api crates/workspaces/src/api.rs
git commit -m "Split the /v1 handlers into one module per resource"
```

---

### Task 11: Delete `VolumeSource::RestoreOf` (audit cut b)

**Files:**
- Modify: `crates/workspaces/src/crd.rs:88-104` (the variant), `:130` (the `RestoreWish.owner` comment referencing it)
- Modify: `deploy/k3s/crds.yaml` (regenerated)
- Test: `crates/workspaces/tests/crd_yaml.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: **the variant simply disappears from the enum.** The agent plan removes `bins/agent/src/controller/volume.rs:268,269,593`, `engine::ops::RESTORE_OF_GONE` (`crates/workspaces/src/engine/ops.rs:18-23`) and the `RestoreMechanismGone` reason string. If that plan has not landed, this task's `cargo test -p kloudlite-git-agent-bin` gate fails to compile — in that case delete the agent's arms here too, in the same commit, and tell the agent plan's executor.

**Why it is safe:** `/v1` has not written the variant since Task 8 of the commit-model work; the cluster was created 2026-08-27 and every object was recreated on 2026-09-03, so no stored spec carries it. A tolerance protecting nothing is a match arm every future reader has to understand.

- [ ] **Step 1: Write the failing test**

In `crates/workspaces/tests/crd_yaml.rs`:

```rust
/// The tolerance for a pre-cutover restore source protected objects that no longer exist. A schema
/// that still advertises the variant invites the next reader to write one.
#[test]
fn restore_of_is_gone_from_the_published_schema() {
    let yaml = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/k3s/crds.yaml")).unwrap();
    assert!(!yaml.contains("restoreOf"), "regenerate deploy/k3s/crds.yaml");
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p kloudlite-git-workspaces --test crd_yaml restore_of_is_gone -- --test-threads=1; echo exit=$?`
Expected: FAIL — `restoreOf` is in the manifest.

- [ ] **Step 3: Delete the variant**

Remove the `RestoreOf { … }` arm and its doc comment from `VolumeSource`. In `RestoreWish.owner`'s doc (`:130`), replace "same rule as `VolumeSource::RestoreOf`" with the rule stated directly: "Absent means the destination's own owner, which is every personal restore."

- [ ] **Step 4: Regenerate and run**

Run:
```
CRD_REGEN=1 cargo test -p kloudlite-git-workspaces --test crd_yaml; echo exit=$?
cargo test -p kloudlite-git-workspaces -p kloudlite-git-agent-bin -- --test-threads=1; echo exit=$?
cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?
```
Expected: exit=0 all three.

- [ ] **Step 5: Commit**

```bash
git add crates/workspaces/src/crd.rs deploy/k3s/crds.yaml crates/workspaces/tests/crd_yaml.rs
git commit -m "Delete the pre-cutover RestoreOf volume source"
```

---

### Task 12: The `/v1` minors — validation, casing, docs and warnings (M1–M5, M7–M10)

**Files:**
- Modify: `crates/workspaces/src/api/workspaces.rs` (`attach_ws`, `create_ws`, `list_ws`, `stop_ws`, `patch_ws_packages`, `clone_ws`, `restore_ws`)
- Modify: `crates/workspaces/src/api/environments.rs` (`start_env`, `stop_env`, `delete_env`)
- Modify: `crates/workspaces/src/api/volumes.rs` (`live_parents`, the `// ── volumes ──` header, `snapshot_rows`)
- Modify: `crates/workspaces/src/api/push.rs` (`optional_push_message`)
- Modify: `crates/workspaces/src/k8s.rs:1024` (the stale Deployment comment)
- Modify: `crates/workspaces/src/engine/commit.rs:78,87,103` (M6)
- Test: `crates/workspaces/tests/api_user.rs`, `crates/workspaces/tests/engine_commit.rs`

- [ ] **Step 1: Write the failing tests**

```rust
/// M1: the environment id is patched verbatim into a label VALUE. `find_env` succeeding means it is
/// a legal object name today, but that is incidental — the same check `validate_ws_spec` applies at
/// the agent belongs here, so a bad value is a 422 rather than a kube 422 laundered into a 500.
#[tokio::test]
async fn attaching_refuses_an_id_that_is_not_a_label_value() {
    let s = server(vec![get(format!("{API}/workspaces/ws-1"), placed_ws("ws-1", "karthik"))]).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/attach", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"environment": "env-1/../../etc"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 422, "{}", resp.text().await.unwrap());
    assert!(s.rec.calls().iter().all(|c| !c.starts_with("PATCH")), "nothing patched");
}

/// M2: `may_act_on` runs on the raw string and only the ACCEPTED value is lowercased, so `Acme`
/// against directory slug `acme` was a 404 "no such team".
#[tokio::test]
async fn a_team_name_is_matched_case_insensitively() {
    let s = server_with_teams(create_routes(), stub_registry(vec![], vec![]).await).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .json(&json!({"name": "web", "team": "Acme", "region": "centralindia", "quota_gb": 20}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
    assert_eq!(s.rec.sent("POST", &format!("{API}/workspaces"))[0]["spec"]["team"], "acme");
}

/// M3: every mutation response built its doc with an empty pushed set, so `volume` was null even
/// for a volume with fifty pushes — and a client reading that as "never pushed" gets a wrong answer
/// from all seven of them.
#[tokio::test]
async fn a_stop_response_reports_the_volume_it_has() {
    let s = server(vec![
        get(format!("{API}/workspaces/ws-1"), placed_ws("ws-1", "karthik")),
        get(format!("{API}/snapshots"), json!({"apiVersion": "kloudlite-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {},
            "items": [{"metadata": {"name": "ws-1-a", "labels": {"kloudlite-git.io/owner": "karthik"}},
                       "spec": {"volume": "ws-1", "owner": "karthik", "worktree": "ws-1", "parent": ""},
                       "status": {"phase": "ready"}}]})),
        Route { method: "PATCH", path: format!("{API}/workspaces/ws-1"), status: 200, body: placed_ws("ws-1", "karthik") },
    ])
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/stop", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["volume"], "vol/karthik/ws-1", "a pushed volume is not null: {body}");
}

/// M4: `live_parents` keys a BTreeMap by volume, so a volume carrying two worktrees showed only the
/// last one's name and kind — and an environment inserted after a workspace overwrote it.
#[tokio::test]
async fn a_volume_with_two_worktrees_names_the_one_that_owns_it() {
    // in api_volumes.rs: a shared clone plus the source, both on ws-1.
    let mut clone = ws_obj("ws-clone", "karthik", "copy");
    clone["status"]["volumeRef"] = json!("ws-1");
    let s = server(vec![
        kget(SNAPS, snap_list(vec![push("ws-1-a", "ws-1", "karthik", "2026-08-27T09:00:00Z")])),
        kget(format!("{API}/workspaces"), ws_list(vec![ws_obj("ws-1", "karthik", "source"), clone])),
        kget(format!("{API}/environments"), env_list(vec![])),
    ])
    .await;
    let (_, body) = get_json(&s, &token(&s.jwt, "karthik"), "/v1/volumes").await;
    assert_eq!(body[0]["display_name"], "source", "the volume's own parent names it: {body}");
}
```

And in `crates/workspaces/tests/engine_commit.rs` for M6:

```rust
/// M6: `swap_worktree`'s intermediate names are worktree-shaped, so `set_quota_worktrees`
/// qgroup-limits them and every `read_dir` of `live/` counts them as worktrees. A crash between the
/// two renames left one behind indefinitely.
#[test]
fn a_swap_leaves_no_worktree_shaped_leftovers() {
    // Build the two names the swap uses and assert the scanner skips them.
    for n in [restoring_name("ws-1"), before_restore_name("ws-1")] {
        assert!(n.starts_with('.'), "{n} must be skipped by the worktree scanners");
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p kloudlite-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: FAIL on each of the new cases.

- [ ] **Step 3: Make the changes**

- **M1** — in `attach_ws`, before the patch: `if !crate::model::valid_segment_label(&body.environment) { return Err((StatusCode::UNPROCESSABLE_ENTITY, "invalid environment id").into_response()); }`, using the same predicate `model::validate_ws_spec:232` applies (`valid_segment(env) && env.len() <= 63`); if that predicate is not already public, make it so rather than writing a second one.
- **M2** — in `create_ws:547` and `list_ws:734`, lowercase the trimmed value once and run `may_act_on` on the lowercased string.
- **M3** — `stop_ws`, `start_env`, `stop_env`, `delete_env`, `clone_ws`, `patch_ws_packages`, `restore_ws`: replace `&HashSet::new()` with the real pushed set, which each handler can get from the single `pushed_volumes(&s, c, &owner)` call `get_ws` already makes. (Task 13 deletes `pushed_volumes`; when it does, these become the CRD-backed set from the same helper.)
- **M4** — `live_parents` keeps a parent per volume, preferring the one whose own name IS the volume:
  ```rust
  // A volume can carry several worktrees (a shared clone, a restore). The parent that OWNS the
  // volume — the one whose id is the volume's — is what names the row; anything else would let a
  // clone rename its source's listing.
  live.entry(vol).or_insert(parent);
  ```
  with the owning parent inserted first (`insert` for `name == vol`, `or_insert` otherwise).
- **M5** — `restore_ws:1349`: keep `my_ws(&s, &owner, &volume)` and state the assumption in a comment ("an owned volume shares its parent's id, which is the only case this resolves; a shared-clone volume resolves the SOURCE workspace, and the team and region it contributes are the source's on purpose").
- **M7** — `optional_push_message`: drop `async` and the two `.await`s at the call sites.
- **M8** — fix the four stale comments: `k8s.rs:1024` "One StatefulSet per service"; `api/push.rs` "the agent's snapshot reconciler"; the `CommitPending` reference in `clone_env`'s ponytail marker (keep the marker, name the real guard); and the `// ── volumes ──` block header, which must say the records are `Snapshot` CRs in the cluster, not on the server tier.
- **M9** — `snapshot_rows`: keep `"lineage": []` and `"region": ""` only if `web/apps/web/src/lib/api.ts` still names them (grep it); if it does, add the one-line comment saying so; if it does not, delete both keys and note it for the web plan.
- **M10** — `delete_env` and `refresh_user_keys`: on a partial failure attach `body["warning"]`, exactly as `stop_ws` does, saying how many workspaces may still name the deleted environment.
- **M6** — `engine/commit.rs:78,87`: prefix both intermediate names with `.` (`.restoring-{ws}`, `.before-restore-{ws}`) and make the worktree scanners skip a leading dot; fix the "not a registry read" comment at `:103`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p kloudlite-git-workspaces -p kloudlite-git-agent-bin -- --test-threads=1; echo exit=$?` and the clippy gate.
Expected: exit=0 both.

- [ ] **Step 5: Commit**

```bash
git add crates/workspaces/src crates/workspaces/tests
git commit -m "Fix the /v1 minors: attach validation, team casing, volume in mutation responses"
```

---

### Task 13: Delete the dead upstream and registry surface (CL1–CL5)

**Files:**
- Modify: `crates/workspaces/src/upstream.rs:26,81-88,92-134`
- Modify: `crates/workspaces/src/registry.rs:95-104` and the constants that go with it
- Modify: `crates/workspaces/src/api/mod.rs` (`ApiState::upstream`, `with_upstream`), `api/scope.rs` or `api/workspaces.rs` (`pushed_volumes`), `api/volumes.rs` (`upstream_err`)
- Modify: `bins/api/src/main.rs` (the `with_upstream` wiring)
- Modify: `crates/workspaces/tests/api_user.rs` (`server_with_registry`, `server_with_teams`, `stub_registry` uses)

**Interfaces:**
- Consumes: Task 10's module layout.
- Produces: `pushed_volumes` is gone. Its one caller-visible effect — whether `ws_doc.volume`/`env_doc.volume` is `Some` — is answered by a CRD-backed `async fn pushed_volumes(s: &ApiState, c: &kube::Client, owners: &[String]) -> Result<HashSet<String>, Response>` that lists `Snapshot`s by the owner label, applies `mine`, keeps `is_snapshot()` rows in any phase but `Error`, and collects `spec.volume`. Same name, same signature shape, no HTTP round trip.

**Why:** `list_volumes` is CRD-backed already, so the peer round trip per `list_ws`/`get_ws` — and per *owner* in `list_env` — buys one nullable string that the `Snapshot` list beside it already answers. `Upstream::history`, `Provenance`, `VolumeRow.latest_ms` and five `VolExt` methods then have no callers at all.

- [ ] **Step 1: Write the failing test**

```rust
/// The volume pointer on a workspace doc is answered by the snapshots in the cluster, not by a
/// round trip to the git tier's peer listener: a push writes a Snapshot CR and nothing else.
#[tokio::test]
async fn a_workspace_doc_reports_its_volume_without_an_upstream() {
    let s = server(vec![
        get(format!("{API}/workspaces/ws-1"), placed_ws("ws-1", "karthik")),
        get(format!("{API}/snapshots"), json!({"apiVersion": "kloudlite-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {},
            "items": [{"metadata": {"name": "ws-1-a", "labels": {"kloudlite-git.io/owner": "karthik"}},
                       "spec": {"volume": "ws-1", "owner": "karthik", "worktree": "ws-1", "parent": ""},
                       "status": {"phase": "ready"}}]})),
    ])
    .await; // no `with_upstream` at all
    let resp = reqwest::Client::new()
        .get(format!("{}/v1/workspaces/ws-1", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["volume"], "vol/karthik/ws-1", "{body}");
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p kloudlite-git-workspaces --test api_user a_workspace_doc_reports_its_volume -- --test-threads=1; echo exit=$?`
Expected: FAIL — with no upstream configured the CRD fallback exists, but the assertion pins the behaviour this task makes unconditional; it fails once `with_upstream` is removed from `bins/api` unless the fallback becomes the only path.

- [ ] **Step 3: Delete**

- `upstream.rs`: delete `history` (`:81-88`), `Provenance` and its impl (`:92-134`), the `provenance_reads_past_unrelated_state_and_tolerates_none` test, and `VolumeRow.latest_ms` (`:26`).
- `registry.rs`: trim `VolExt` to `vol_exists`, `history`, `volume_marker_prefix`; delete `append_commits`, `move_ref`, `ref_commit`, `commit`, `region`, and with them `volume_marker`, `REGION_KEY`, `ref_key`, `commit_key`. Leave the read half and the `volumes.rs:3-9` keep-until-drained ruling alone.
- `api`: delete the `upstream` branch of `pushed_volumes` (keep the CRD path as the whole function, with `mine` from Task 4), `ApiState::upstream`, `with_upstream`, `upstream_err`.
- `bins/api/src/main.rs`: delete the `with_upstream` call. `KLOUDLITE_GIT_UPSTREAM`/`KLOUDLITE_GIT_PEER_SECRET` are still needed by `kloudlite_git_api::serve` — leave them.
- tests: fold `server_with_registry` and `server_with_teams` into `server`/`server_with_teams(routes)`, deleting the `stub_registry` arguments; `kube_test::stub_registry` itself goes if nothing else uses it (grep first).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p kloudlite-git-workspaces -p kloudlite-git-agent-bin -- --test-threads=1; echo exit=$?` and the clippy gate.
Expected: exit=0 both.

- [ ] **Step 5: Commit**

```bash
git add -A crates/workspaces bins/api
git commit -m "Answer a workspace's volume pointer from the cluster and delete the upstream detour"
```

---

### Task 14: `live_state`, `hex::encode`, and the two duplicated helpers (CL6, CL8, CL9, audit cut c and d)

**Files:**
- Modify: `crates/workspaces/src/model.rs:79-84` (delete `live_state`), `crates/workspaces/src/api/workspaces.rs` (`ws_doc`'s `live_state: Value::Null`)
- Modify: `crates/workspaces/src/crd.rs:852` (`hex_prefix`), `crates/workspaces/Cargo.toml` (add `hex = { workspace = true }`)
- Modify: `crates/workspaces/src/api/volumes.rs` (merge `find_commit_model_snapshot` and `find_commit_model_snapshot_for_restore`)
- Modify: `crates/workspaces/src/api/push.rs` (`create_snapshot` calls `refuse_cut_in_flight`)
- Test: `crates/workspaces/src/crd.rs` unit tests, `crates/workspaces/tests/api_user.rs`

**Interfaces:**
- Consumes: Task 10's module layout.
- Produces: `async fn find_snapshot(s: &ApiState, caller_id: &str, volume: Option<&str>, snapshot_id: &str) -> Result<crd::Snapshot, Response>` — `Some(volume)` is the restore-in-place check, `None` the restore-to-new one. **`Workspace.live_state` is deleted**; the web plan deletes `web/apps/web/src/lib/api.ts:709`.

- [ ] **Step 1: Write the failing tests**

```rust
// crd.rs tests
/// `hex` is already a workspace dependency; a hand-rolled `format!("{b:02x}")` fold is the same
/// bytes with more places to get it wrong. The tail must not move — it is in stored object names.
#[test]
fn the_namespace_tail_is_unchanged_by_the_hex_swap() {
    assert_eq!(ws_namespace("bob", "acme"), "wt-bob-0d4a1ff8d9c1");
}
```

(Run the current implementation once to read the real expected value and paste it in — a hash whose value the test invents proves nothing.)

```rust
// api_user.rs
/// `live_state` was always `null` and existed only because a web type named it. The web type goes
/// in the web plan; the field goes here, so the two cannot outlive the reason together.
#[tokio::test]
async fn a_workspace_doc_has_no_live_state_field() {
    let s = server(vec![
        get(format!("{API}/workspaces/ws-1"), placed_ws("ws-1", "karthik")),
        get(format!("{API}/snapshots"), json!({"apiVersion": "kloudlite-git.io/v1alpha1", "kind": "SnapshotList", "metadata": {}, "items": []})),
    ])
    .await;
    let body: Value = reqwest::Client::new()
        .get(format!("{}/v1/workspaces/ws-1", s.base))
        .bearer_auth(token(&s.jwt, "karthik"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(body.get("live_state").is_none(), "{body}");
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p kloudlite-git-workspaces -- --test-threads=1; echo exit=$?`
Expected: FAIL on `a_workspace_doc_has_no_live_state_field`; the hash test passes and is the guard for step 3.

- [ ] **Step 3: Make the four changes**

- `hex_prefix` becomes `hex::encode(&sha2::Sha256::digest(raw.as_bytes())[..bytes])`; add `hex = { workspace = true }` to `crates/workspaces/Cargo.toml`.
- Delete `Workspace.live_state` and its `ws_doc` line.
- Merge the two resolvers into `find_snapshot(s, caller_id, volume: Option<&str>, snapshot_id)`, whose volume clause is `if volume.is_some_and(|v| snap.spec.volume != v)`; update `restore_env_in_place` (`Some(&volume)`), `restore_ws` and `restore_env` (`None`).
- `create_snapshot` drops its inline `racing` block and calls `refuse_cut_in_flight(&all, worktree)` after the same `spec.volume` list, keeping the `// ponytail:` TOCTOU marker on the helper (one copy, not two).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p kloudlite-git-workspaces -p kloudlite-git-agent-bin -- --test-threads=1; echo exit=$?` and the clippy gate.
Expected: exit=0 both — including the unchanged namespace tail, which is what proves the hex swap is a refactor.

- [ ] **Step 5: Commit**

```bash
git add crates/workspaces
git commit -m "Delete live_state and fold the duplicated snapshot resolver and cut guard"
```

---

### Task 15: The vocabulary pass — "commit" becomes "snapshot" (CL10, CL11)

**Files:**
- Modify: `crates/workspaces/src/api/volumes.rs`, `api/push.rs`, `crates/workspaces/src/crd.rs`, `crates/workspaces/src/engine/commit.rs` → `crates/workspaces/src/engine/snapshot.rs`, `crates/workspaces/src/engine/ops.rs`, `crates/workspaces/src/registry.rs`, `crates/workspaces/src/k8s.rs`
- Modify: `bins/agent/**` call sites of any renamed item
- Modify: `crates/workspaces/tests/engine_commit.rs` → `engine_snapshot.rs`, `tests/api_commit_model.rs` → `tests/api_snapshots.rs`

**Interfaces:**
- Consumes: everything above.
- Produces: renames only. **`VolumeSource::CloneOf { commit }` keeps its field name** — it is on stored CRs; if it must read better, add `#[serde(alias = "commit")]` and rename the Rust field, never the wire name. `crd::commit_labels` → `crd::snapshot_labels`; `commit_model_snapshots*` → `snapshots_for_caller` / `snapshots_for_caller_maybe_empty`; `engine::commit` → `engine::snapshot`; `commit_worktree` → `snapshot_worktree`; `local_commits` → `local_snapshots`; `drop_commit` → `drop_snapshot`.

**No behaviour change.** A rename that also fixes something is a rename nobody can review.

- [ ] **Step 1: Enumerate what moves**

Run: `grep -rn "commit" crates/workspaces/src bins/agent/src | grep -v "CloneOf" | grep -v "git commit"`
Record the list; every hit is either renamed or is a `commit` that genuinely means a git commit (the merge worker's, in another crate).

- [ ] **Step 2: Rename, one identifier at a time, compiling between each**

Run after each: `cargo check -p kloudlite-git-workspaces -p kloudlite-git-agent-bin; echo exit=$?`
Expected: exit=0 each time.

- [ ] **Step 3: Strip the sixteen `Task N` references (CL11)**

At `api.rs`'s six sites (now in `api/workspaces.rs` and `api/volumes.rs`), `crd.rs:89,243`, `engine/ops.rs:19,168`, `engine/snapshot.rs:149`, `registry.rs:30`, `k8s.rs:1511,1549,1560`: replace "Task N" with the fact itself, or with a citation of `docs/superpowers/specs/2026-09-03-durable-snapshots-design.md`. The plans those numbers name are not in the tree.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p kloudlite-git-workspaces -p kloudlite-git-agent-bin -- --test-threads=1; echo exit=$?` and the clippy gate.
Expected: exit=0 both, with the same number of tests passing as before the rename.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "Rename the internal commit vocabulary to snapshot"
```

---

### Task 16: Lift the naming helpers out of `crd.rs` (CL12)

**Files:**
- Create: `crates/workspaces/src/crd/names.rs`
- Modify: `crates/workspaces/src/crd.rs` → `crates/workspaces/src/crd/mod.rs`

**Interfaces:**
- Consumes: Task 14's `hex::encode`, Task 15's renames.
- Produces: no public change — `crd::ws_namespace`, `crd::env_namespace`, `crd::binding_name` all still resolve, via `pub use names::*;` in `mod.rs`.

- [ ] **Step 1: Move the unit**

Move `ws_namespace`, `env_namespace`, `binding_name`, `dns_label`, `pair_tail`, `hex_prefix` and their tests into `crd/names.rs`, with the module doc stating why they are a unit: they are the deterministic name derivations `/v1` and the agent must both compute identically, and every one of them has had a bug where two distinct pairs collided.

- [ ] **Step 2: Re-export and compile**

Run: `cargo check -p kloudlite-git-workspaces -p kloudlite-git-agent-bin; echo exit=$?`
Expected: exit=0 with `pub use names::*;` in `crd/mod.rs`.

- [ ] **Step 3: Run the tests**

Run:
```
cargo test -p kloudlite-git-workspaces -p kloudlite-git-agent-bin -- --test-threads=1; echo exit=$?
cargo clippy --workspace --all-targets --locked -- -D warnings; echo exit=$?
CRD_REGEN=1 cargo test -p kloudlite-git-workspaces --test crd_yaml; echo exit=$?
```
Expected: exit=0 all three, `deploy/k3s/crds.yaml` unchanged (a pure move must not move the manifest).

- [ ] **Step 4: Commit**

```bash
git add crates/workspaces/src/crd crates/workspaces/src/crd.rs
git commit -m "Move the name derivations into crd/names.rs"
```

---

## Self-Review

**1. Coverage of `workspaces-api.md`:** C1 → Task 1. C2 → Tasks 2 (`/v1` cap) and 3 (ResourceQuota). I1, I2 → Task 4. I3 → Task 5. I4 → Task 6. I5 → Task 7. I6 → Task 8. I7 → Task 10. M1–M5, M7–M10 and M6 → Task 12. CL1–CL5 → Task 13. CL6, CL8, CL9 → Task 14. CL7 → Task 6. CL10, CL11 → Task 15. CL12 → Task 16. Audit cuts: (a) → Task 9, (b) → Task 11, (c) → Task 14, (d) → Task 14. Nothing in the "what is good and should not be touched" list is modified: `validate_mount`/`validate_service` and their agent-side re-checks, `packages.rs`, the `parents_of_volume`/`live_parents` split (Task 7 changes how it queries, not its opposite biases), `lenient_state`, `is_snapshot`, the recorder harness, `owners_namespaces`, `replicate::targets`, the fixed-string error hygiene and `hardened()` all stand.

**2. Spec coverage:** the durable-snapshots rules the plan touches are rule 5 (delete is the only explicit verb; Tasks 1, 5, 7 all strengthen its refusals without widening what it deletes) and rule 6 (restore re-attaches; Task 8 refuses a cross-kind graft, which the spec's restore table assumes). The snapshot-state design's "data, not authorization" rule is what Task 8 implements — a state is validated, and now also typed, before it reaches a spec. No spec requirement is left without a task.

**3. Ordering hazards:** Task 6 edits `model::Region` that Task 9 deletes — deliberate, because the review's order puts I4 before the Region CRD, and Task 6 is two field deletions plus a projection, all of which Task 9 carries forward. Task 11's gate depends on the agent plan; the task says what to do if that plan has not landed. Tasks 12 and 13 both touch `ws_doc`'s pushed set — Task 12 passes the real set, Task 13 changes where it comes from; Task 12 notes it.

**4. Type consistency:** `snapshots_on_volume` (Task 1) is the unfiltered listing throughout; `mine`/`Owned` (Task 4) is the filter throughout; `Parent { kind, display, head, base }` is one shape from Task 5 onward, with `source_snapshot`/`on_volume` introduced in Task 5 and only re-called in Task 7; `ApiState::new(jwt, admins)` is the post-Task-9 constructor everywhere; `find_snapshot(s, caller_id, Option<&str>, id)` (Task 14) replaces both resolvers, and Task 15 renames `commit_model_snapshots*` but not it.
