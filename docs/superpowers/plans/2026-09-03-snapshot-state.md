# Snapshot State Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every `Snapshot` freezes its parent's definition (image, packages, resources, quota, attachment for a workspace; services and quota for an environment) beside the bytes, and restore defaults to it.

**Architecture:** One optional field, `SnapshotSpec.state: Option<SnapshotState>`, a tagged enum derived from the parent spec by two constructors. All five cut sites stamp it (two in `/v1`, three in the agent; the agent's listing derives it for free). Restore builds the new spec by precedence body → snapshot → live source → defaults, validating everything the same way create does. History rows expose it; the web summarises it and pre-fills restore.

**Tech Stack:** Rust (kube-rs, serde, schemars), the existing `crates/workspaces` CRD/API crates and `bins/agent`, Next.js web app (`bun test`).

**Spec:** `docs/superpowers/specs/2026-09-03-snapshot-state-design.md`

## Global Constraints

- `SnapshotState` is `#[serde(tag = "kind", rename_all = "camelCase")]` with variants `Workspace { image, packages, resources, quota_gb, attached_environment }` and `Environment { services, quota_gb }`; the JSON tag values are exactly `"workspace"` and `"environment"`; field names on the wire are `quotaGb`, `attachedEnvironment`.
- `SnapshotSpec.state` is `Option<SnapshotState>` with `#[serde(default, skip_serializing_if = "Option::is_none")]` — a snapshot cut before this change still deserializes, and every reader has a fallback for `None`.
- The state is a COPY frozen at cut time; no reader ever follows it back to the live object.
- Restore precedence, both kinds: explicit request body field → snapshot `state` → live source's spec (if it still exists) → the kind's defaults. `RestoreEnvBody.services` absent ⇒ snapshot's services; `[]` explicit ⇒ no services.
- Everything a restore takes from a snapshot passes the same validation as a request body: `crate::packages::validate_list` for packages, `check_services` for services. A snapshot is data, never an authorization; owner and team never come from the state.
- `quota_gb` in the state is `spec.storage.quota_gb`, else the kind's default (`default_env_quota()` for environments; the workspace default `/v1` applies at create).
- `deploy/k3s/crds.yaml` is regenerated with `CRD_REGEN=1 cargo test -p kloudlite-workspaces --test crd_yaml` and committed; never hand-edited.
- Comments say why, never what; keep every `// ponytail:` marker; commit subjects imperative sentence case; no tool attribution anywhere in commit messages.
- Gates for every task: `cargo test -p kloudlite-agent-bin -p kloudlite-workspaces -- --test-threads=1; echo exit=$?` (unpiped) and `cargo clippy --workspace --all-targets --locked -- -D warnings`; web tasks add `cd web && bun run lint && bunx tsc --noEmit -p apps/web/tsconfig.json && bun test`.

---

### Task 1: The `SnapshotState` type, its two constructors, and the CRD field

**Files:**
- Modify: `crates/workspaces/src/crd.rs` (`SnapshotSpec` at ~238; add the enum beside it)
- Modify: `deploy/k3s/crds.yaml` (regenerated)
- Test: `crates/workspaces/src/crd.rs` `mod tests`, `crates/workspaces/tests/crd_yaml.rs`

**Interfaces:**
- Produces: `pub enum SnapshotState { Workspace { image: String, packages: Vec<String>, resources: PodResources, quota_gb: u64, attached_environment: Option<String> }, Environment { services: Vec<crate::model::Service>, quota_gb: u64 } }`; `impl SnapshotState { pub fn of_workspace(w: &Workspace) -> Self; pub fn of_environment(e: &Environment) -> Self }`; `SnapshotSpec.state: Option<SnapshotState>`.

- [ ] **Step 1: Write the failing tests** — in `crates/workspaces/src/crd.rs` `mod tests`:

```rust
#[test]
fn snapshot_state_serializes_with_the_kind_tag_and_camel_case() {
    let st = SnapshotState::Workspace {
        image: "alpine:3.20".into(),
        packages: vec!["ripgrep".into()],
        resources: PodResources::default(),
        quota_gb: 5,
        attached_environment: Some("env-1".into()),
    };
    let v = serde_json::to_value(&st).unwrap();
    assert_eq!(v["kind"], "workspace");
    assert_eq!(v["quotaGb"], 5);
    assert_eq!(v["attachedEnvironment"], "env-1");
    let back: SnapshotState = serde_json::from_value(v).unwrap();
    assert_eq!(back, st);
}

#[test]
fn a_snapshot_spec_without_state_still_deserializes() {
    let s: SnapshotSpec = serde_json::from_value(serde_json::json!({
        "volume": "v", "owner": "o", "worktree": "v", "parent": "", "pinned": false, "transient": false
    }))
    .unwrap();
    assert!(s.state.is_none());
    // and a None state is not written at all
    assert!(serde_json::to_value(&s).unwrap().get("state").is_none());
}

#[test]
fn of_workspace_copies_the_spec_and_falls_back_to_the_default_quota() {
    let mut w = Workspace::new("ws-1", WorkspaceSpec {
        owner: "o".into(), team: String::new(), name: "n".into(), region: "r".into(),
        image: "alpine:3.20".into(), storage: None, desired_state: DesiredState::Running,
        resources: PodResources::default(), packages: vec!["jq".into()], attached_environment: None,
    });
    match SnapshotState::of_workspace(&w) {
        SnapshotState::Workspace { image, packages, quota_gb, attached_environment, .. } => {
            assert_eq!(image, "alpine:3.20"); assert_eq!(packages, vec!["jq"]);
            assert_eq!(quota_gb, DEFAULT_WS_QUOTA_GB); assert_eq!(attached_environment, None);
        }
        other => panic!("{other:?}"),
    }
    w.spec.storage = Some(WorkspaceStorage { quota_gb: 42, source: None });
    assert!(matches!(SnapshotState::of_workspace(&w), SnapshotState::Workspace { quota_gb: 42, .. }));
}
```

If `WorkspaceSpec` has fields this literal does not name, fill them with their defaults; if no `DEFAULT_WS_QUOTA_GB` constant exists, introduce `pub const DEFAULT_WS_QUOTA_GB: u64` in `crd.rs` holding the value `/v1`'s create path applies today (find it: `grep -n quota crates/workspaces/src/api.rs` around `create_ws`) and make that path use the constant.

- [ ] **Step 2: Run them, expect failure** — `cargo test -p kloudlite-workspaces snapshot_state of_workspace a_snapshot_spec_without` fails: `SnapshotState` not found.

- [ ] **Step 3: Implement** — in `crd.rs`, beside `SnapshotSpec`:

```rust
/// What the parent WAS when this cut was taken, frozen beside the bytes. A restore defaults to
/// it, which is the whole reason it exists: last month's files with today's image is not last
/// month's workspace. A copy, never a reference — later edits to the parent leave it alone.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SnapshotState {
    #[serde(rename_all = "camelCase")]
    Workspace {
        image: String,
        packages: Vec<String>,
        resources: PodResources,
        quota_gb: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attached_environment: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Environment {
        services: Vec<crate::model::Service>,
        quota_gb: u64,
    },
}

impl SnapshotState {
    pub fn of_workspace(w: &Workspace) -> Self {
        SnapshotState::Workspace {
            image: w.spec.image.clone(),
            packages: w.spec.packages.clone(),
            resources: w.spec.resources.clone(),
            quota_gb: w.spec.storage.as_ref().map(|s| s.quota_gb).unwrap_or(DEFAULT_WS_QUOTA_GB),
            attached_environment: w.spec.attached_environment.clone(),
        }
    }
    pub fn of_environment(e: &Environment) -> Self {
        SnapshotState::Environment {
            services: e.spec.services.clone(),
            quota_gb: e.spec.storage.as_ref().map(|s| s.quota_gb).unwrap_or(DEFAULT_ENV_QUOTA_GB),
        }
    }
}
```

Add to `SnapshotSpec`:

```rust
    /// Absent only on a snapshot cut before 2026-09-03; every reader falls back for `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<SnapshotState>,
```

Move the environment default quota next to the workspace one as `pub const DEFAULT_ENV_QUOTA_GB: u64` and make `api.rs`'s `default_env_quota()` return it. `model::Service` and `PodResources` must derive `JsonSchema` and `PartialEq` — add the derives if missing (check `Mount` too). Every `SnapshotSpec { .. }` literal in the tree (five cut sites, tests) needs `state: None` for now — grep `SnapshotSpec {` and add it; later tasks replace the `None`s.

- [ ] **Step 4: Regenerate the CRD and run the gates** — `CRD_REGEN=1 cargo test -p kloudlite-workspaces --test crd_yaml`, then `cargo test -p kloudlite-agent-bin -p kloudlite-workspaces -- --test-threads=1; echo exit=$?` and clippy. Confirm `deploy/k3s/crds.yaml` now carries `state` with a `kind` discriminator under the Snapshot schema (`grep -c attachedEnvironment deploy/k3s/crds.yaml` ≥ 1).

- [ ] **Step 5: Commit** — `git add crates/workspaces deploy/k3s/crds.yaml bins/agent && git commit -m "Add the snapshot state record to the Snapshot CRD"`

---

### Task 2: `/v1` stamps state on push and clone cuts

**Files:**
- Modify: `crates/workspaces/src/api.rs` — `create_commit` (~1784), `push_ws`/`push_env` (~1754/1768), `clone_base` (~1150-1195)
- Test: `crates/workspaces/tests/api_commit_model.rs`

**Interfaces:**
- Consumes: `crd::SnapshotState::{of_workspace, of_environment}` (Task 1).
- Produces: `create_commit(c, volume, owner, worktree, parent, message, state: crd::SnapshotState)`; `clone_base(.., state: crd::SnapshotState)` (or it derives from `src` internally — pick the one with fewer call-site edits and say which in the report).

- [ ] **Step 1: Write the failing tests** — in `crates/workspaces/tests/api_commit_model.rs`, following the file's existing recorder pattern (`rec.sent("POST", "/apis/kloudlite.io/v1alpha1/snapshots")`):

```rust
#[tokio::test]
async fn a_push_records_the_workspace_definition_on_the_snapshot() {
    // fixture: a Ready workspace ws-1 with image "alpine:3.20", packages ["jq"], a volume, head None
    let (app, rec) = app_with_ready_workspace("ws-1", "alpine:3.20", &["jq"]).await;
    let r = post(&app, "/v1/workspaces/ws-1/push", json!({"message": "m"})).await;
    assert_eq!(r.status(), 202);
    let body = rec.sent("POST", "/apis/kloudlite.io/v1alpha1/snapshots").pop().unwrap();
    assert_eq!(body["spec"]["state"]["kind"], "workspace");
    assert_eq!(body["spec"]["state"]["image"], "alpine:3.20");
    assert_eq!(body["spec"]["state"]["packages"], json!(["jq"]));
}

#[tokio::test]
async fn a_push_records_the_environment_services_on_the_snapshot() {
    let (app, rec) = app_with_ready_environment("env-1", 2).await; // two services
    let r = post(&app, "/v1/environments/env-1/push", json!({})).await;
    assert_eq!(r.status(), 202);
    let body = rec.sent("POST", "/apis/kloudlite.io/v1alpha1/snapshots").pop().unwrap();
    assert_eq!(body["spec"]["state"]["kind"], "environment");
    assert_eq!(body["spec"]["state"]["services"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn a_clone_cut_records_the_source_definition() {
    let (app, rec) = app_with_ready_workspace("ws-1", "alpine:3.20", &["jq"]).await;
    let r = post(&app, "/v1/workspaces/ws-1/clone", json!({"name": "c"})).await;
    assert_eq!(r.status(), 202);
    let cut = rec.sent("POST", "/apis/kloudlite.io/v1alpha1/snapshots").into_iter()
        .find(|b| b["metadata"]["name"].as_str().unwrap().starts_with("clone-")).unwrap();
    assert_eq!(cut["spec"]["state"]["kind"], "workspace");
    assert_eq!(cut["spec"]["state"]["image"], "alpine:3.20");
}
```

Use the file's existing fixture helpers for a ready workspace/environment (names above are illustrative — reuse what `api_commit_model.rs` already has, e.g. the helpers the `based_on` tests use; do not invent parallel fixtures).

- [ ] **Step 2: Run them, expect failure** — the POSTed spec has no `state` (or `state: null`).

- [ ] **Step 3: Implement** — `create_commit` gains `state: crd::SnapshotState` and sets `state: Some(state)` in the `SnapshotSpec`; `push_ws` passes `crd::SnapshotState::of_workspace(&w)`, `push_env` passes `crd::SnapshotState::of_environment(&e)`. In `clone_base`, the cut's spec gets `state: Some(crd::SnapshotState::of_workspace(src))` (the source Workspace is already in scope at the call site as `src`; thread it or its state through). Remove the `state: None` placeholders Task 1 left in these two sites.

- [ ] **Step 4: Run the gates.**

- [ ] **Step 5: Commit** — `git commit -am "Record the parent's definition on push and clone cuts"`

---

### Task 3: The agent stamps state on stop, sync and baseline cuts

**Files:**
- Modify: `bins/agent/src/listing.rs` (`Parent` at ~30; the two builders at ~100-125)
- Modify: `bins/agent/src/sync.rs` (~53-71), `bins/agent/src/controller/stop.rs` (`stop_push` ~15-95), `bins/agent/src/controller/workspace.rs` (`migrate_and_seed_baseline` ~608-632 and its callers at ~823 and `environment.rs:246`)
- Test: `bins/agent/src/sync.rs` `mod tests`, `bins/agent/src/controller/stop.rs` tests (or `bins/agent/tests/reconcile.rs` where the stop cut is asserted), `bins/agent/tests/reconcile.rs` for the baseline

**Interfaces:**
- Consumes: `crd::SnapshotState` (Task 1).
- Produces: `listing::Parent.state: crd::SnapshotState`; `stop_push(.., state: crd::SnapshotState)`; `migrate_and_seed_baseline(ctx, vol, owner, state: crd::SnapshotState)`.

- [ ] **Step 1: Write the failing tests**

In `bins/agent/src/sync.rs` tests (the existing sync-cut test that asserts the POSTed `SnapshotSpec` — extend it):

```rust
// alongside the existing assertions on the sync cut body
assert_eq!(body["spec"]["state"]["kind"], "workspace");
assert_eq!(body["spec"]["state"]["image"], "alpine:3.20"); // whatever image the test's Parent fixture carries
```

The `Parent` fixture in those tests gains `state: crd::SnapshotState::Workspace { image: "alpine:3.20".into(), packages: vec![], resources: Default::default(), quota_gb: 5, attached_environment: None }`.

In `bins/agent/tests/reconcile.rs`, the existing stop-cut test (grep `stop-ws-` in that file) gains:

```rust
let cut = rec.sent("POST", "/apis/kloudlite.io/v1alpha1/snapshots").into_iter()
    .find(|b| b["metadata"]["name"].as_str().unwrap().starts_with("stop-")).unwrap();
assert_eq!(cut["spec"]["state"]["kind"], "workspace");
assert_eq!(cut["spec"]["state"]["packages"], serde_json::json!(ws_fixture_packages()));
```

and the baseline test (grep `migration baseline` or `the_migration_baseline_is_owned_by_its_volume`) gains `assert_eq!(body["spec"]["state"]["kind"], "workspace");`. Add the environment twin for the stop cut if the file has an environment stop test (`stop-env-`): assert `kind == "environment"` and the services length.

- [ ] **Step 2: Run them, expect failure** — `state` absent on all three cuts.

- [ ] **Step 3: Implement**
  - `listing::Parent` gains `pub state: crd::SnapshotState`; the Workspace builder sets `crd::SnapshotState::of_workspace(w)`, the Environment builder `of_environment(e)`.
  - `sync.rs`: the cut's spec gets `state: Some(live.state.clone())`.
  - `stop.rs`: `stop_push` gains `state: crd::SnapshotState`; the two callers pass `of_workspace(w)` / `of_environment(e)`; the cut's spec gets `state: Some(state)`.
  - `migrate_and_seed_baseline` gains `state: crd::SnapshotState`; `workspace.rs:823` passes `of_workspace(w)`, `environment.rs:246` passes `of_environment(e)`; the baseline spec gets `state: Some(state)`. The volume.rs test at ~815 that calls it directly passes a literal `SnapshotState::Workspace { .. }`.
  - Remove the remaining `state: None` placeholders from Task 1 in these files.

- [ ] **Step 4: Run the gates.**

- [ ] **Step 5: Commit** — `git commit -am "Record the parent's definition on stop, sync and baseline cuts"`

---

### Task 4: Restore defaults to the snapshot's state

**Files:**
- Modify: `crates/workspaces/src/api.rs` — `RestoreBody` (~1291), `restore_ws` (~1300-1372), `RestoreEnvBody` (~1464), `restore_env` (~1490-1560)
- Test: `crates/workspaces/tests/api_commit_model.rs`

**Interfaces:**
- Consumes: `SnapshotSpec.state` (Task 1); `crate::packages::validate_list`, `check_services`, `default_ws_image`, `DEFAULT_WS_QUOTA_GB`, `DEFAULT_ENV_QUOTA_GB`, `clamp_quota`.
- Produces: `RestoreBody { name, snapshot_id, image: Option<String>, packages: Option<Vec<String>>, resources: Option<PodResources>, quota_gb: Option<u64>, attached_environment: Option<String> }`; `RestoreEnvBody.services: Option<Vec<Service>>`, `quota_gb: Option<u64>`.

- [ ] **Step 1: Write the failing tests** — in `api_commit_model.rs`, with a Snapshot fixture that carries `spec.state` (workspace: image `"alpine:3.19"`, packages `["ripgrep"]`, quotaGb 7; environment: two services, quotaGb 9):

```rust
#[tokio::test]
async fn restoring_a_workspace_whose_source_is_gone_takes_the_snapshot_state() {
    let (app, rec) = app_with_snapshot_only("ws-src-aaaa", workspace_state("alpine:3.19", &["ripgrep"], 7)).await;
    let r = post(&app, "/v1/workspaces/restore", json!({"name": "back", "snapshot_id": "ws-src-aaaa"})).await;
    assert_eq!(r.status(), 202);
    let body = rec.sent("POST", "/apis/kloudlite.io/v1alpha1/workspaces").pop().unwrap();
    assert_eq!(body["spec"]["image"], "alpine:3.19");
    assert_eq!(body["spec"]["packages"], json!(["ripgrep"]));
    assert_eq!(body["spec"]["storage"]["quotaGb"], 7);
}

#[tokio::test]
async fn a_restore_body_field_overrides_the_snapshot_state() {
    let (app, rec) = app_with_snapshot_only("ws-src-aaaa", workspace_state("alpine:3.19", &["ripgrep"], 7)).await;
    let r = post(&app, "/v1/workspaces/restore", json!({"name": "back", "snapshot_id": "ws-src-aaaa", "image": "alpine:3.20"})).await;
    assert_eq!(r.status(), 202);
    let body = rec.sent("POST", "/apis/kloudlite.io/v1alpha1/workspaces").pop().unwrap();
    assert_eq!(body["spec"]["image"], "alpine:3.20");
    assert_eq!(body["spec"]["packages"], json!(["ripgrep"])); // untouched fields still come from the snapshot
}

#[tokio::test]
async fn a_pre_change_snapshot_restores_as_before() {
    let (app, rec) = app_with_snapshot_only("ws-src-aaaa", /* state */ None).await;
    let r = post(&app, "/v1/workspaces/restore", json!({"name": "back", "snapshot_id": "ws-src-aaaa"})).await;
    assert_eq!(r.status(), 202);
    let body = rec.sent("POST", "/apis/kloudlite.io/v1alpha1/workspaces").pop().unwrap();
    assert_eq!(body["spec"]["image"], kloudlite_workspaces::model::default_ws_image());
    assert_eq!(body["spec"]["packages"], json!([]));
}

#[tokio::test]
async fn a_snapshot_with_a_bad_package_name_is_refused_like_a_bad_body() {
    let (app, _rec) = app_with_snapshot_only("ws-src-aaaa", workspace_state("alpine:3.19", &["../evil"], 7)).await;
    let r = post(&app, "/v1/workspaces/restore", json!({"name": "back", "snapshot_id": "ws-src-aaaa"})).await;
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn restoring_an_environment_without_services_takes_the_snapshots() {
    let (app, rec) = app_with_env_snapshot_only("env-src-aaaa", environment_state(2, 9)).await;
    let r = post(&app, "/v1/environments/restore", json!({"name": "back", "snapshot_id": "env-src-aaaa"})).await;
    assert_eq!(r.status(), 202);
    let body = rec.sent("POST", "/apis/kloudlite.io/v1alpha1/environments").pop().unwrap();
    assert_eq!(body["spec"]["services"].as_array().unwrap().len(), 2);
    assert_eq!(body["spec"]["storage"]["quotaGb"], 9);
}

#[tokio::test]
async fn restoring_an_environment_with_empty_services_restores_data_only() {
    let (app, rec) = app_with_env_snapshot_only("env-src-aaaa", environment_state(2, 9)).await;
    let r = post(&app, "/v1/environments/restore", json!({"name": "back", "snapshot_id": "env-src-aaaa", "services": []})).await;
    assert_eq!(r.status(), 202);
    let body = rec.sent("POST", "/apis/kloudlite.io/v1alpha1/environments").pop().unwrap();
    assert_eq!(body["spec"]["services"].as_array().unwrap().len(), 0);
}
```

`app_with_snapshot_only` / `app_with_env_snapshot_only`: reuse the restore fixtures the file already has for "source deleted" (grep `restore` in the file); `workspace_state`/`environment_state` are small local helpers returning `serde_json::Value` for the Snapshot fixture's `spec.state`. Keep the existing restore tests passing (they cover the live-source path).

- [ ] **Step 2: Run them, expect failure** — the restored spec ignores the snapshot state; the env restore without `services` fails deserialization (`services` was required-with-default).

- [ ] **Step 3: Implement**

`RestoreBody`:

```rust
struct RestoreBody {
    name: String,
    snapshot_id: String,
    #[serde(default)] image: Option<String>,
    #[serde(default)] packages: Option<Vec<String>>,
    #[serde(default)] resources: Option<crd::PodResources>,
    #[serde(default)] quota_gb: Option<u64>,
    #[serde(default)] attached_environment: Option<String>,
}
```

In `restore_ws`, after `snap` and `src` are resolved:

```rust
// Precedence: the request, then what the snapshot froze, then the live source, then defaults.
// A snapshot's state is data — it goes through the same checks a body does, below.
let frozen = match &snap.spec.state {
    Some(crd::SnapshotState::Workspace { image, packages, resources, quota_gb, attached_environment }) =>
        Some((image.clone(), packages.clone(), resources.clone(), *quota_gb, attached_environment.clone())),
    _ => None,
};
let image = body.image.clone()
    .or_else(|| frozen.as_ref().map(|f| f.0.clone()))
    .or_else(|| src.as_ref().map(|w| w.spec.image.clone()))
    .unwrap_or_else(default_ws_image);
let packages = body.packages.clone()
    .or_else(|| frozen.as_ref().map(|f| f.1.clone()))
    .or_else(|| src.as_ref().map(|w| w.spec.packages.clone()))
    .unwrap_or_default();
let resources = body.resources.clone()
    .or_else(|| frozen.as_ref().map(|f| f.2.clone()))
    .or_else(|| src.as_ref().map(|w| w.spec.resources.clone()))
    .unwrap_or_default();
let quota = match (body.quota_gb, &frozen, &src) {
    (Some(q), _, _) => clamp_quota(q),
    (None, Some(f), _) => f.3,
    (None, None, Some(w)) => storage_quota(c, &w.spec.storage, &volume).await,
    (None, None, None) => crd::DEFAULT_WS_QUOTA_GB,
};
let attached_environment = body.attached_environment.clone()
    .or_else(|| frozen.as_ref().and_then(|f| f.4.clone()));
crate::packages::validate_list(&packages).map_err(bad_packages)?;
```

then use these five in the `WorkspaceSpec` literal that today hard-codes `resources: Default::default()`, `packages: vec![]`, `attached_environment: None`. Apply whatever image check `create_ws` applies (grep it) to `image` here too. If `attached_environment` names an environment the caller cannot see, drop it (`None`) — the same rule `/v1` applies at create; find that check and reuse it.

`RestoreEnvBody`: `services: Option<Vec<Service>>`, `quota_gb: Option<u64>`. In `restore_env`:

```rust
let frozen = match &snap.spec.state {
    Some(crd::SnapshotState::Environment { services, quota_gb }) => Some((services.clone(), *quota_gb)),
    _ => None,
};
let services = body.services.clone().or_else(|| frozen.as_ref().map(|f| f.0.clone())).unwrap_or_default();
check_services(&services)?;
let quota = match (body.quota_gb, &frozen) { (Some(q), _) => clamp_quota(q), (None, Some(f)) => f.1, (None, None) => default_env_quota() };
```

Move the existing `check_services(&body.services)?` call to after this resolution (it must validate the resolved list, whichever source it came from). Update the doc comment on `restore_env` that says a snapshot does not record services.

- [ ] **Step 4: Run the gates.**

- [ ] **Step 5: Commit** — `git commit -am "Restore from a snapshot's frozen definition, body fields overriding"`

---

### Task 5: History rows expose the state

**Files:**
- Modify: `crates/workspaces/src/api.rs` — `commit_model_history_rows` (~2245-2265), the `Workspace.live_state` doc comment in `crates/workspaces/src/model.rs` (~80)
- Test: `crates/workspaces/tests/api_commit_model.rs`

**Interfaces:**
- Consumes: `SnapshotSpec.state`.
- Produces: history/refs rows carry `"state": <SnapshotState JSON> | null`.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn history_rows_carry_the_frozen_state_or_null() {
    let (app, _rec) = app_with_history(vec![
        ready_snapshot("ws-1-aaaa", Some(workspace_state("alpine:3.19", &["jq"], 5))),
        ready_snapshot("ws-1-bbbb", None),
    ]).await;
    let r = get(&app, "/v1/volumes/ws-1/history").await;
    let rows: Vec<serde_json::Value> = body_json(r).await;
    let by_id = |id: &str| rows.iter().find(|r| r["id"] == id).unwrap().clone();
    assert_eq!(by_id("ws-1-aaaa")["state"]["kind"], "workspace");
    assert_eq!(by_id("ws-1-aaaa")["state"]["image"], "alpine:3.19");
    assert!(by_id("ws-1-bbbb")["state"].is_null());
}
```

(`app_with_history`/`ready_snapshot` = whatever helper the file's existing history tests use; extend it to accept a state.)

- [ ] **Step 2: Run it, expect failure** — `state` is `null` on both rows.

- [ ] **Step 3: Implement** — in `commit_model_history_rows` replace `"state": serde_json::Value::Null` with `"state": serde_json::to_value(&sn.spec.state).unwrap_or(serde_json::Value::Null)` (an `Option::None` serializes to `null`). Rewrite the `live_state` doc comment in `model.rs` to: the per-snapshot definition is on the history rows' `state`; this field stays `null` and is kept only because the web types name it.

- [ ] **Step 4: Run the gates.**

- [ ] **Step 5: Commit** — `git commit -am "Expose each snapshot's frozen definition on history rows"`

---

### Task 6: Web — summary line and restore pre-fill

**Files:**
- Modify: `web/apps/web/src/lib/api.ts` (`ApiCommitRecord` ~944: type `state`; `restoreWorkspace`/`restoreEnvironment` calls gain optional fields)
- Create: `web/apps/web/src/lib/snapshot-state.ts`, `web/apps/web/src/lib/snapshot-state.test.ts`
- Modify: `web/apps/web/src/app/(shell)/[owner]/(org)/workspaces/[id]/snapshots/page.tsx` (row rendering ~52), `web/apps/web/src/components/app/env-snapshots.tsx` (its row), `web/apps/web/src/components/app/restore-dialog.tsx`, `web/apps/web/src/app/(shell)/[owner]/(org)/workspaces/actions.ts` (`restoreWorkspace` ~56)

**Interfaces:**
- Consumes: history rows' `state` (Task 5); restore body fields (Task 4).
- Produces: `export type SnapshotState = { kind: "workspace"; image: string; packages: string[]; quotaGb: number; attachedEnvironment?: string | null; resources: {...} } | { kind: "environment"; services: {name: string; image: string}[]; quotaGb: number }`; `export function stateSummary(s: SnapshotState | null | undefined): string` (`""` for none).

- [ ] **Step 1: Write the failing tests** — `web/apps/web/src/lib/snapshot-state.test.ts` (`bun:test`, same shape as `ws-status.test.ts`):

```ts
import { describe, expect, test } from "bun:test";
import { stateSummary } from "./snapshot-state";

describe("stateSummary", () => {
  test("a workspace names its image and package count", () => {
    expect(stateSummary({ kind: "workspace", image: "alpine:3.20", packages: ["jq", "rg", "fd", "bat"], quotaGb: 5, resources: {} as never })).toBe("alpine:3.20 · 4 packages");
    expect(stateSummary({ kind: "workspace", image: "alpine:3.20", packages: ["jq"], quotaGb: 5, resources: {} as never })).toBe("alpine:3.20 · 1 package");
    expect(stateSummary({ kind: "workspace", image: "alpine:3.20", packages: [], quotaGb: 5, resources: {} as never })).toBe("alpine:3.20");
  });
  test("an environment counts its services", () => {
    expect(stateSummary({ kind: "environment", services: [{ name: "db", image: "x" }, { name: "api", image: "y" }, { name: "web", image: "z" }], quotaGb: 5 })).toBe("3 services");
    expect(stateSummary({ kind: "environment", services: [{ name: "db", image: "x" }], quotaGb: 5 })).toBe("1 service");
  });
  test("no state renders nothing", () => {
    expect(stateSummary(null)).toBe("");
    expect(stateSummary(undefined)).toBe("");
  });
});
```

- [ ] **Step 2: Run it, expect failure** — `cd web && bun test snapshot-state` → module not found.

- [ ] **Step 3: Implement**
  - `lib/snapshot-state.ts`: the `SnapshotState` type above and `stateSummary` (`n === 1 ? "1 package" : `${n} packages``; same for services; empty packages ⇒ image only).
  - `lib/api.ts`: `ApiCommitRecord.state: SnapshotState | null`; `restoreWorkspace(token, name, snapshotId, extra?: { image?: string; packages?: string[] })` and the environment twin `extra?: { services?: ... }` pass the extras in the body when given.
  - Workspace snapshots page and `env-snapshots.tsx`: render `stateSummary(c.state)` as a second line under the message in the row (`text-sm2`, muted token), omitted when empty. Copy the sibling row-detail pattern in `workspace-list.tsx`.
  - `restore-dialog.tsx`: accept `state?: SnapshotState | null`; when a workspace state is present, render editable `image` and `packages` (comma-separated) inputs pre-filled from it, and a read-only line for quota; the action `restoreWorkspace` reads those fields and passes them as `extra` (only when the person changed them or they are non-empty — pass them always is fine: same values as the snapshot). Environment restore dialog (wherever `restoreEnvironment` is triggered — grep) shows the service names read-only and passes nothing (the API defaults to the snapshot).

- [ ] **Step 4: Gates** — `cd web && bun run lint && bunx tsc --noEmit -p apps/web/tsconfig.json && bun test`.

- [ ] **Step 5: Commit** — `git add web && git commit -m "Show each snapshot's frozen definition and pre-fill restore from it"`

---

### Task 7: Docs and the e2e assertion

**Files:**
- Modify: `CLAUDE.md` (the "Four verbs" paragraph — add one sentence: every cut records `spec.state`, the parent's definition at that instant, and restore defaults to it)
- Modify: `deploy/k3s/README.md` (a short "Release: snapshot state" note after the dead-field one: the CRD apply adds an optional field, so it is order-free relative to the agent roll; old snapshots simply have no state)
- Modify: `tests/ws_e2e.sh` (after the existing push assertion for the workspace: restore from that snapshot with the source deleted and assert the image matches)

- [ ] **Step 1: Write the e2e assertion** — in `tests/ws_e2e.sh`, after the workspace push block (grep `push` and the snapshot id variable it records):

```bash
# The snapshot froze the definition: a restore with the source GONE must come back with the
# source's image, not the default one.
SRC_IMAGE=$(kubectl get workspace "$WS" -o jsonpath='{.spec.image}')
api DELETE "/v1/workspaces/$WS" >/dev/null
RS=$(api POST /v1/workspaces/restore "{\"name\":\"e2e-restore\",\"snapshot_id\":\"$SNAP\"}" | jq -r .id)
for i in $(seq 1 60); do [ "$(kubectl get workspace "$RS" -o jsonpath='{.status.phase}' 2>/dev/null)" = ready ] && break; sleep 5; done
[ "$(kubectl get workspace "$RS" -o jsonpath='{.spec.image}')" = "$SRC_IMAGE" ] || fail "restore did not take the snapshot's image"
[ "$(kubectl get snapshot "$SNAP" -o jsonpath='{.spec.state.kind}')" = workspace ] || fail "snapshot carries no state"
```

Match the script's own helper names (`api`, `fail`, how it parses ids) — read the surrounding block first and copy its idioms; if the script deletes `$WS` later, move that delete here.

- [ ] **Step 2: `bash -n tests/ws_e2e.sh`** — clean.

- [ ] **Step 3: Docs** — the two edits above; grep every name you write (`spec.state`, `SnapshotState`) exists in the code.

- [ ] **Step 4: Commit** — `git commit -am "Document the snapshot state record and assert it end to end"`

---

## Self-review

- **Spec coverage:** record (Task 1); five cut sites (Tasks 2–3); restore precedence, validation, `services` absent-vs-empty (Task 4); history (Task 5); web summary + pre-fill (Task 6); `live_state` comment (Task 5); docs + e2e (Task 7); CRD regen (Task 1). Clone unchanged — no task, by design. Cases table: dead-source restore (Task 4 test 1), env without services (Task 4 test 5), `services: []` (test 6), pre-change snapshot (test 3), bad package (test 4); missing attached environment handled in Task 4 Step 3.
- **Placeholders:** fixture helper names in Tasks 2, 4, 5 are marked "reuse the file's existing helpers" with the grep to find them; no TBDs.
- **Type consistency:** `SnapshotState::{of_workspace, of_environment}` (Task 1) used in Tasks 2–3; `SnapshotSpec.state: Option<SnapshotState>` everywhere; `DEFAULT_WS_QUOTA_GB`/`DEFAULT_ENV_QUOTA_GB` introduced in Task 1, used in Task 4; web `SnapshotState`/`stateSummary` in Task 6 only.
