# Phase 2B: k3s Controller Migration — Implementation Plan

> **For agentic workers:** execute this with superpowers:subagent-driven-development — one task per
> subagent, in order, each finishing with its stated verification output pasted back.

**Goal:** move the workspace/environment control plane off the hand-rolled Cosmos job queue and onto
the Kubernetes API. `/v1` writes CRDs; a per-node controller in `kloudlite-agent` watches only its
own node's objects and reconciles btrfs + pods; `Job`/`JobKind`/`JobState`, `lease.rs`, `spawn_sweep`,
the four `/vol-agent` job routes, `AgentDoc` and heartbeats are deleted. The btrfs engine
(`engine/{ops,blob,pool,fsck}.rs`) and the registry `vol/{owner}/{id}` surface do not change.

**Architecture:**

```
bins/api (/v1)  --writes CRD spec-->  k3s API server  --watch spec.nodeName==$NODE_NAME-->  kloudlite-agent (DaemonSet)
      ^                                    |                                                    |
      |                                    | status subresource                                 +-- btrfs (Engine, spawn_blocking)
      +--- list/get read status back <-----+                                                    +-- Namespace/Pod/Deployment/Service/NetworkPolicy
                                                                                                +-- registry vol/{owner}/{id} (unchanged, over HTTP)
```

Three cluster-scoped CRDs in `kloudlite.io/v1alpha1`: `Volume` (btrfs lifecycle), `Workspace`
(one pinned Pod), `Environment` (one namespace of Deployments+Services), plus a fourth,
`OwnerBinding`, replacing `Binding` in Cosmos. `Region` stays in Cosmos and is the only thing left
there that this plan reads.

**Tech Stack:** Rust 1.97, `kube` 4.2.0 (features `derive`, `runtime`, default `client`+`rustls-tls`+`ring`),
`k8s-openapi` 0.28.0 (feature `v1_33`), `schemars` 1, existing `tokio`/`axum`/`serde`. Every kube-rs
signature quoted below was read out of the vendored `.crate` sources for **kube 4.2.0 / kube-runtime
4.2.0 / kube-client 4.2.0 / kube-core 4.2.0 / kube-derive 4.2.0**, not from memory.

**Spec:** `docs/superpowers/specs/2026-08-26-k3s-architecture-design.md`
**Infrastructure half (do not duplicate):** `docs/superpowers/plans/2026-08-26-phase2a-k3s-bootstrap.md`

---

## ⚠️ Cross-plan blocker found while resolving versions — read before Task 1

`k8s-openapi` **0.28.0** (the only version `kube` 4.2.0 accepts — its `Cargo.toml` pins
`[dependencies.k8s-openapi] version = "0.28.0"`) exposes exactly these Kubernetes-version features:
`v1_32, v1_33, v1_34, v1_35, v1_36` (plus `earliest`/`latest`). **There is no `v1_31`.** Phase 2A Task
3 Step 2 installs `INSTALL_K3S_VERSION=v1.31.5+k3s1`.

Resolution, in preference order:

1. **Bump Phase 2A to `v1.33.x+k3s1`** and compile with `k8s-openapi` feature `v1_33`. This is the
   recommendation: CRD `selectableFields` needs ≥1.30 either way, and a client one minor *behind*
   the server is the supported direction, not one *ahead*.
2. If the cluster must stay on 1.31, pin `kube` 2.x/3.x against `k8s-openapi` 0.26 instead — but then
   **every kube-rs signature in this plan must be re-verified**, because they were read from 4.2.0.

Do not paper over this with `earliest` (that is `v1_32`, still ahead of a 1.31 server). Edit Phase
2A's `INSTALL_K3S_VERSION` in the same commit as Task 1 and say so in the commit body.

**What could not be verified:** nothing else. crates.io's JSON API refused requests from this
environment, so versions came from the sparse index (`index.crates.io`) and all API shapes from the
downloaded `.crate` tarballs. If a subagent hits a compile error against a signature quoted here,
re-read the vendored source under `~/.cargo/registry/src/*/kube-*-4.2.0/` rather than guessing.

---

## Global Constraints

- `cargo clippy --workspace -- -D warnings` passes at every commit.
- `cargo test` passes at **every commit**. Deleting a subsystem the API depends on means the two
  commits that do it (Task 5, then Task 7) delete their own tests in the same commit as the code.
  Task 5 is the one place both paths exist at once — see its note.
- Comments explain WHY, never what; match the density of `bins/server/src/router/route.rs`.
- Deliberate shortcuts carry `// ponytail: <ceiling and upgrade path>`; keep existing markers.
- Commit subjects imperative sentence case, no tool attribution.
- **`crate::model::validate_mount` runs on every path that turns a `Mount` into a hostPath.** A pod
  spec is submitted to an API server that will happily mount `/`. This rule is never relaxed.
- `engine/{ops,blob,pool,fsck}.rs` change only where Task 2 says, and the registry volume-record
  routes (`vol_agent.rs`'s `commits`/`ref`/`history`, `VOL_AGENT_TAILS`) are not touched at all.

---

## File Structure

| Path | Responsibility |
| --- | --- |
| `crates/workspaces/src/crd.rs` | **NEW.** `Volume`/`Workspace`/`Environment`/`OwnerBinding` `v1alpha1` types via `#[derive(CustomResource)]`, status types, conditions helper, `all_crds()` for YAML generation. |
| `crates/workspaces/tests/crd_yaml.rs` | **NEW.** Golden-file test that regenerates and diffs `deploy/k3s/crds.yaml` — the artifact Phase 2A Task 6 installs. |
| `deploy/k3s/crds.yaml` | **NEW (generated).** The `v1 List` of four CRDs. Never hand-edited. |
| `crates/workspaces/src/k8s.rs` | **NEW.** Pure functions building `Namespace`/`Pod`/`Deployment`/`Service`/`NetworkPolicy` from `Service`/`Mount`/CRD specs. Holds the C1 regression test migrated out of `compose.rs`. |
| `crates/workspaces/src/placement.rs` | **NEW.** `bind_owner`/`place` against `OwnerBinding` + node allocatable-minus-requests. Replaces `scheduler.rs`'s Cosmos half. |
| `crates/workspaces/src/api.rs` | **MODIFIED.** `/v1` writes CRDs and reads status back. `ApiState.store` narrows to regions only; gains `ApiState.kube`. |
| `crates/workspaces/src/model.rs` | **MODIFIED.** `Job`/`JobKind`/`JobState`/`AgentDoc`/`Capacity`/`Binding` deleted. `Region`, `Service`, `Mount`, `validate_mount`, `Snapshot`, `LineageEntry`, `WsState`, `EnvState`, `Workspace`, `Environment`, `default_ws_image` stay. |
| `crates/workspaces/src/store.rs` | **MODIFIED.** `MetaStore` loses 10 methods (agents, bindings, jobs); `MemStore` shrinks with it. Workspace/env/snapshot methods stay (the engine still uses them). |
| `crates/workspaces/src/cosmos.rs` | **MODIFIED.** Same 10 impls removed. Containers `agents`/`jobs`/`bindings` stop being created. |
| `crates/workspaces/src/scheduler.rs` | **DELETED.** Owner binding moves to `placement.rs`; the job-placement CAS half dies with `Job`. |
| `crates/workspaces/src/lease.rs` | **DELETED.** |
| `crates/workspaces/src/engine/compose.rs` | **DELETED.** Its `render` mount-validation test moves to `k8s.rs` first (Task 3), not last. |
| `crates/workspaces/src/engine/ops.rs` | **MODIFIED (Task 2 only).** `create_subvol`/`clone_local_snapshot`/`clone_running_local` tolerate an existing `live` (H2's surviving half). |
| `crates/workspaces/src/lib.rs` | **MODIFIED.** `mod` list follows the above. |
| `bins/agent/src/controller.rs` | **NEW.** The three reconcilers, the `{uid, generation}` single-flight map, `spawn_blocking` dispatch, finalizer handling, status writes. |
| `bins/agent/src/lib.rs` | **MODIFIED.** `Config`, `blob_store`, `build_engine`, `meta_store_from_env`, `owner_file`, the janitor and `cleanup_local` survive. `register`, `run_with_engine`, `run_job*`, `report`, `docker_stop_name`/`docker_start_name`/`compose`, `JOB_CPU`/`JOB_MEM_MB`, `agent_id_path` deleted. |
| `bins/agent/src/container.rs` | **DELETED.** |
| `bins/agent/tests/loop.rs` | **DELETED** (Task 5) — it drives `/vol-agent/work`, which ceases to exist. |
| `bins/agent/tests/reconcile.rs` | **NEW.** Reconcile unit tests against a mocked `kube::Client`. |
| `bins/server/src/vol_agent.rs` | **MODIFIED.** `register`/`work`/`job_done`/`job_failed`/`spawn_sweep`/`JobsState.store`/`mark_ws_*`/`mark_env_*`/`region_by_token` deleted. `JobsState` keeps only the token check for the record routes. |
| `bins/server/src/boot.rs` | **MODIFIED.** `build_jobs_state` stops opening a Cosmos store and stops spawning the sweep. |
| `bins/server/src/router/mod.rs` | **MODIFIED.** Drops `.merge(vol_agent_job_routes())`. |
| `bins/api/src/main.rs` | **MODIFIED.** Builds a `kube::Client`; the Cosmos store is constructed only for `Region`. |
| `crates/workspaces/tests/{api_user,api_teams,api_volumes}.rs` | **REWRITTEN** against the mocked kube client. |
| `tests/vol_agent.rs` | **MODIFIED.** Job-route tests deleted; record-route tests kept. |
| `tests/common/mod.rs` | **MODIFIED.** `jobs_state_with_store` deleted, `no_jobs_state` kept. |
| `tests/ws_e2e.sh` | **REWRITTEN** against a k3s cluster and `kubectl`. |

---

### Task 1: CRD types and generated `crds.yaml`

Additive only — nothing existing changes behaviour, so `cargo test` is green trivially. This is what
Phase 2A Task 6 installs, so it lands first.

**Files:** `Cargo.toml`, `crates/workspaces/Cargo.toml`, `crates/workspaces/src/crd.rs`,
`crates/workspaces/src/lib.rs`, `crates/workspaces/tests/crd_yaml.rs`, `deploy/k3s/crds.yaml`,
`docs/superpowers/plans/2026-08-26-phase2a-k3s-bootstrap.md` (the k3s version bump).

**Interfaces** (consumed by every later task):

```rust
// crates/workspaces/src/crd.rs
pub const GROUP: &str = "kloudlite.io";
pub const VERSION: &str = "v1alpha1";
pub const FIELD_MANAGER: &str = "kloudlite";          // /v1 writes spec under this
pub const AGENT_FIELD_MANAGER: &str = "kloudlite-agent"; // the controller writes status under this
pub const SUBVOLUME_FINALIZER: &str = "kloudlite.io/subvolume";

pub struct VolumeSpec  { pub owner: String, pub node_name: String, pub region: String,
                         pub quota_gb: u64, pub source: Option<VolumeSource> }
pub enum   VolumeSource { CloneOf { volume: String }, RestoreOf { volume: String, snapshot_id: String } }
pub struct VolumeStatus { pub phase: String, pub observed_generation: Option<i64>,
                          pub subvolume_present: bool, pub lineage_tip: Option<String>,
                          pub last_push: Option<LastPush>, pub progress: Option<String>,
                          pub conditions: Vec<Condition> }
pub struct WorkspaceSpec { pub owner: String, pub name: String, pub region: String, pub image: String,
                           pub volume_ref: String, pub node_name: String,
                           pub desired_state: DesiredState, pub resources: PodResources }
pub struct WorkspaceStatus { pub phase: String, pub observed_generation: Option<i64>,
                             pub pod_ref: Option<String>, pub conditions: Vec<Condition> }
pub struct EnvironmentSpec { pub owner: String, pub name: String, pub region: String,
                             pub services: Vec<crate::model::Service>, pub volume_ref: String,
                             pub node_name: String, pub desired_state: DesiredState }
pub struct EnvironmentStatus { pub phase: String, pub observed_generation: Option<i64>,
                               pub service_status: Vec<ServiceStatus>, pub conditions: Vec<Condition> }
pub struct OwnerBindingSpec { pub owner: String, pub region: String, pub node_name: String }

pub enum DesiredState { Running, Stopped }

/// `{region}-{owner}` lowercased — the RFC-1123 object name for an owner's node binding.
pub fn binding_name(region: &str, owner: &str) -> String;
/// Namespace an object's children live in: `ws-{id}` / `env-{id}`.
pub fn ws_namespace(id: &str) -> String;
pub fn env_namespace(id: &str) -> String;
/// Every CRD this repo owns, for YAML generation and for a startup precondition check.
pub fn all_crds() -> Vec<k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition>;
/// Standard condition helper — `Ready`/`Progressing`/`Degraded`, observedGeneration stamped.
pub fn condition(kind: &str, status: bool, reason: &str, message: &str, gen: i64) -> Condition;
```

`Condition` is `k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition` (re-exported from
`crd.rs`), not a bespoke struct — it already has the shape `kubectl wait --for=condition=Ready`
reads.

- [ ] **Step 1:** Failing test first. Create `crates/workspaces/tests/crd_yaml.rs`:
  ```rust
  //! `deploy/k3s/crds.yaml` is a GENERATED artifact — Phase 2A installs exactly what the Rust
  //! types say. This test is the generator (`CRD_REGEN=1 cargo test -p kloudlite-workspaces
  //! --test crd_yaml`) and the drift check in one, so a field added to a spec struct cannot ship
  //! without the manifest moving with it.
  use kloudlite_workspaces::crd::all_crds;

  #[test]
  fn generated_crds_match_the_committed_manifest() {
      // A `v1 List` of JSON, written to a `.yaml` path on purpose: YAML is a superset of JSON, so
      // `kubectl apply -f` accepts it verbatim, and this keeps the archived `serde_yaml`
      // (RUSTSEC-2024-0320) out of the tree. ponytail: unreadable diffs; swap to `serde-saphyr`
      // if a human ever has to review this file by eye.
      let doc = serde_json::json!({"apiVersion": "v1", "kind": "List", "items": all_crds()});
      let want = format!("{}\n", serde_json::to_string_pretty(&doc).unwrap());
      let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/k3s/crds.yaml");
      if std::env::var("CRD_REGEN").is_ok() {
          std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap()).unwrap();
          std::fs::write(path, &want).unwrap();
      }
      let got = std::fs::read_to_string(path).unwrap_or_default();
      assert_eq!(got, want, "run CRD_REGEN=1 cargo test --test crd_yaml to regenerate");
  }

  #[test]
  fn every_crd_has_a_status_subresource_and_a_nodename_field_selector() {
      // Both are load-bearing and both fail SILENTLY if dropped: without `status: {}` a status
      // update is folded into spec (and Phase 2A's RBAC split becomes decorative); without
      // `selectableFields` every node's controller sees every node's work and two agents race
      // the same subvolume.
      for crd in all_crds() {
          let v = &crd.spec.versions[0];
          assert!(v.subresources.as_ref().is_some_and(|s| s.status.is_some()), "{}", crd.spec.names.kind);
          if crd.spec.names.kind == "OwnerBinding" { continue; } // not watched per-node
          let sel = v.selectable_fields.as_ref().expect("selectableFields");
          assert!(sel.iter().any(|f| f.json_path == ".spec.nodeName"), "{}", crd.spec.names.kind);
      }
  }
  ```
  Command: `cargo test -p kloudlite-workspaces --test crd_yaml`
  Expected failure: `error[E0433]: failed to resolve: could not find 'crd' in 'kloudlite_workspaces'`.
- [ ] **Step 2:** Add the dependencies. In the root `Cargo.toml` `[workspace.dependencies]`:
  ```toml
  # The Kubernetes API is the reconcile substrate (Phase 2B). kube 4.2 pins k8s-openapi 0.28
  # exactly, and 0.28 has no `v1_31` feature — which is why deploy/k3s installs k3s v1.33.
  # A client one minor behind the server is supported; ahead is not.
  kube = { version = "4.2", features = ["derive", "runtime"] }
  k8s-openapi = { version = "0.28", features = ["v1_33"] }
  schemars = "1"
  ```
  `kube`'s default features (`client`, `rustls-tls`, `ring`) are kept deliberately: this workspace
  already installs a **ring** rustls provider in `main()`, and letting kube pull `aws-lc-rs` would
  put a second TLS stack in the graph — the same rule the root `rustls` comment states.
  Add `kube`, `k8s-openapi`, `schemars` to `crates/workspaces/Cargo.toml` `[dependencies]`.
  Verify: `cargo tree -p kloudlite-workspaces -i k8s-openapi | head -3` names 0.28.0 exactly once,
  and `cargo tree -d | grep -c rustls` does not increase versus `git stash`ed HEAD.
- [ ] **Step 3:** Write `crates/workspaces/src/crd.rs`. Shape, `Volume` in full — the other three
  follow it exactly:
  ```rust
  use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
  use kube::CustomResource;
  use schemars::JsonSchema;
  use serde::{Deserialize, Serialize};

  /// `camelCase` is not cosmetic: the field selector below indexes the literal JSON path
  /// `.spec.nodeName`, and `selectableFields` is matched as a string by the API server.
  #[derive(CustomResource, Clone, Debug, Serialize, Deserialize, JsonSchema)]
  #[kube(
      group = "kloudlite.io",
      version = "v1alpha1",
      kind = "Volume",
      plural = "volumes",
      shortname = "vol",
      status = "VolumeStatus",
      selectable = ".spec.nodeName",
      printcolumn = r#"{"name":"Owner","type":"string","jsonPath":".spec.owner"}"#,
      printcolumn = r#"{"name":"Node","type":"string","jsonPath":".spec.nodeName"}"#,
      printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
      printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#,
      derive = "PartialEq"
  )]
  #[serde(rename_all = "camelCase")]
  pub struct VolumeSpec {
      pub owner: String,
      /// Written ONCE by the /v1 admission path from the owner's `OwnerBinding`. The pod's
      /// affinity is derived from this and never chosen independently — two places allowed to
      /// name a node is two places that can disagree about where the data is.
      pub node_name: String,
      pub region: String,
      pub quota_gb: u64,
      #[serde(default, skip_serializing_if = "Option::is_none")]
      pub source: Option<VolumeSource>,
  }
  ```
  Notes that are easy to get wrong and are all verified against the vendored macro:
  - `#[kube(status = "…")]` is what emits `subresources.status` (kube-derive
    `custom_resource.rs:836`); there is no separate `subresource` attribute.
  - `#[kube(selectable = "…")]` emits `SelectableField { json_path }` (same file, line 832). Pass
    the leading dot — it is copied verbatim into the CRD.
  - Omitting `namespaced` makes the CRD **cluster-scoped**, which all four are.
  - `derive = "PartialEq"` is required for the root type to be comparable; the controller's
    "did status actually change" guard (Task 6) needs it to avoid a self-triggering write loop.
  - `schemars` honours `#[serde(rename_all)]`, so the generated OpenAPI schema is camelCase too;
    do not add a separate `#[schemars(rename_all)]`.
  `VolumeSource` is `#[serde(rename_all = "camelCase")]` on an enum with struct variants —
  `{"cloneOf": {"volume": "..."}}`. Reuse `crate::model::Service`/`Mount` verbatim inside
  `EnvironmentSpec`; that requires adding `JsonSchema` to their derive list in `model.rs` (the only
  change `model.rs` takes in this task).
  Then `pub mod crd;` in `lib.rs`.
- [ ] **Step 4:** Generate and commit the manifest.
  `CRD_REGEN=1 cargo test -p kloudlite-workspaces --test crd_yaml && cargo test -p kloudlite-workspaces --test crd_yaml`
  Expected: first run writes, second run passes both tests with no `CRD_REGEN`.
  Sanity-check the artifact matches what Phase 2A Task 6 Step 5 demands:
  `grep -c '"jsonPath": ".spec.nodeName"' deploy/k3s/crds.yaml` → `3`.
- [ ] **Step 5:** Edit the Phase 2A plan: `INSTALL_K3S_VERSION=v1.31.5+k3s1` → `v1.33.5+k3s1` in
  Task 3 Step 2 and Task 4 Step 3, and the Task 3 Step 6 expectation `{"major":"1","minor":"31"` →
  `"minor":"33"`. Add a one-line WHY next to the version naming the `k8s-openapi` 0.28 floor.
  Verify: `grep -c 'v1.31' docs/superpowers/plans/2026-08-26-phase2a-k3s-bootstrap.md` → `0`.
- [ ] **Step 6:** `cargo clippy --workspace -- -D warnings && cargo test`
  Expected: clean, all pre-existing tests still pass.
  Commit: `Define the v1alpha1 workspace CRDs and generate deploy/k3s/crds.yaml`

---

### Task 2: Carry over the surviving audit fixes (H3, H2's idempotent half)

Independent of k8s and small — landing it early means the migration is not also carrying two bug
fixes. The spec calls H3 a hard prerequisite for rebuilding a node.

**Files:** `crates/workspaces/src/engine/ops.rs`, `bins/agent/src/lib.rs`,
`crates/workspaces/tests/engine_ops.rs`.

**Interfaces:** none change. `Engine::pull_env(&self, owner: &str, id: &str)` already has the right
signature — only its one call site is wrong.

- [ ] **Step 1:** Failing test for H2 in `crates/workspaces/tests/engine_ops.rs` (btrfs-gated the
  same way every test in that file is — `if !have_btrfs() { return }`):
  ```rust
  /// A controller restart re-runs reconcile from scratch, so create/clone against a subvolume that
  /// already exists must be a no-op, not an error that marks a healthy workspace Error. This is
  /// the half of audit H2 that survives deleting the lease.
  #[tokio::test]
  async fn create_and_clone_are_idempotent_against_an_existing_live_subvolume() {
      // create_subvol twice, then clone_local_ids twice, asserting Ok(()) each time and that the
      // second call left the subvolume's contents alone (write a marker file between calls).
  }
  ```
  Command: `sudo -E cargo test -p kloudlite-workspaces --test engine_ops idempotent`
  Expected failure on a btrfs box: `ERROR: cannot create subvolume - already exists` surfaced as
  `EngErr`. On this Mac the test skips — say so and run it on the btrfs VM.
- [ ] **Step 2:** Make `create_subvol` tolerant (`ops.rs:166`):
  ```rust
  pub fn create_subvol(&self, id: &str) -> Result<(), EngErr> {
      std::fs::create_dir_all(self.pool.voldir(id)).map_err(EngErr::io)?;
      // Reconcile is level-triggered and a restarted controller replays it from scratch, so an
      // existing `live` is the expected steady state, not a conflict. Keep-biased: never delete
      // and recreate — that would be data loss dressed up as convergence.
      if !self.pool.live(id).exists() {
          run(&["btrfs", "subvolume", "create", self.pool.live(id).to_str().unwrap()])?;
      }
      std::fs::create_dir_all(self.pool.recv()).map_err(EngErr::io)?;
      Ok(())
  }
  ```
  Same guard, same comment reference, in `clone_local_snapshot` (`ops.rs:610`, around the
  `btrfs subvolume snapshot … live(dst_id)` call) and `clone_running_local` (`ops.rs:692`, the
  snapshot inside the `snapshotted` closure). `pull_core` (`ops.rs:476`) **already** has it — do
  not touch that one; point the new comments at it as the precedent.
- [ ] **Step 3:** Fix H3 — `bins/agent/src/lib.rs:516` passes `(&env.id, r)` to a
  `(owner, id)` signature, so the re-materialization path has never once run correctly:
  ```rust
  Some(_) => {
      // H3: this argument pair was `(&env.id, r)` — id-as-owner, ref-as-id — so a node rebuild
      // silently "restored" an environment as an empty subvolume even though its pushed history
      // was sitting in the registry. `volume` being Some is the signal that history EXISTS; the
      // owner/id pair is what addresses it.
      engine.pull_env(&env.owner, &env.id).await.map_err(|e| e.to_string())?;
  }
  ```
  The binding is now unused, so the match becomes `Some(_)`. This code moves into the controller in
  Task 6; fixing it here means the fix is reviewable on its own and survives the move.
- [ ] **Step 4:** `sudo -E cargo test -p kloudlite-workspaces --test engine_ops` on the btrfs VM,
  then `cargo clippy --workspace -- -D warnings && cargo test` locally.
  Expected: the new test passes; nothing else moves.
  Commit: `Make subvolume create and clone tolerate an existing live subvolume`
  (H3 in a second commit: `Fix the swapped owner and id arguments in the environment pull path`)

---

### Task 3: Build Kubernetes objects from the domain types

Pure functions, no client, no I/O — the easiest thing in this plan to test exhaustively, and the
place the C1 regression test lives from now on. It must land **before** `compose.rs` is deleted so
the test never spends a commit not existing.

**Files:** `crates/workspaces/src/k8s.rs`, `crates/workspaces/src/lib.rs`.

**Interfaces:**

```rust
// crates/workspaces/src/k8s.rs
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{Namespace, Pod, Service as CoreService};
use k8s_openapi::api::networking::v1::NetworkPolicy;

/// The pool root every hostPath is built under, from `WS_POOL`. Callers pass it explicitly rather
/// than reading the env inside these functions — a pure builder is what makes them testable.
pub struct PodContext<'a> { pub pool: &'a str, pub node_name: &'a str, pub owner_ref: OwnerReference }

/// `ws-{id}` / `env-{id}` with Pod Security Admission `restricted` and the owner label the
/// NetworkPolicies key on. `restricted` is a LABEL, not code we write.
pub fn namespace(name: &str, owner: &str, kind: &str, owner_ref: &OwnerReference) -> Namespace;

/// The workspace's one pinned Pod. Errors only on an invalid image name — the volume set is built
/// from ids, never from caller-supplied paths.
pub fn workspace_pod(spec: &WorkspaceSpec, id: &str, ctx: &PodContext) -> Pod;

/// One Deployment per `Service`. **Every mount goes through `validate_mount` here** — this is the
/// last place before a string becomes a hostPath, and the API server would mount `/` for us.
pub fn service_deployment(svc: &model::Service, env_id: &str, ctx: &PodContext) -> Result<Deployment, String>;
pub fn service_clusterip(svc: &model::Service, env_id: &str, ctx: &PodContext) -> CoreService;

/// default-deny ingress+egress, allow-dns, allow-same-namespace — the same three Phase 2A Task 8
/// Step 6 templates, generated instead of rendered from YAML so there is one source.
pub fn default_policies(ns: &str, owner_ref: &OwnerReference) -> Vec<NetworkPolicy>;
/// One policy per attachment, in the ENVIRONMENT's namespace, keyed by the workspace namespace's
/// label. Attaching is an authorization decision made in /v1; this only expresses it.
pub fn attach_policy(env_ns: &str, ws_ns: &str, owner_ref: &OwnerReference) -> NetworkPolicy;
```

- [ ] **Step 1:** Failing test — move `compose.rs`'s `render_refuses_a_mount_that_escapes_the_subvolume`
  into `k8s.rs`'s `mod tests`, rewritten against `service_deployment`, plus the pieces the spec
  claims and nothing currently proves:
  ```rust
  #[test]
  fn a_service_deployment_refuses_a_mount_that_escapes_the_subvolume() {
      let ctx = ctx();  // pool /mnt/wspool, node session-0
      let ok = service_deployment(&svc("data", "/data"), "env-1", &ctx).unwrap();
      let vol = &ok.spec.as_ref().unwrap().template.spec.as_ref().unwrap().volumes.as_ref().unwrap()[0];
      assert_eq!(vol.host_path.as_ref().unwrap().path, "/mnt/wspool/vol/env-1/live/volumes/data");
      // `Directory`, never `DirectoryOrCreate`: a missing subvolume must fail the pod loudly
      // rather than silently running against an empty dir k8s made for us (audit H3's shape).
      assert_eq!(vol.host_path.as_ref().unwrap().type_.as_deref(), Some("Directory"));
      // The C1 payload: `{"folder": "/", "path": "/host"}` hostPath-mounts the host root RW into
      // a container whose image the same caller chose.
      for bad in ["/", "..", "a/b", "", "../../root/.ssh", "a:b"] {
          assert!(service_deployment(&svc(bad, "/host"), "env-1", &ctx).is_err(), "folder {bad:?}");
      }
      assert!(service_deployment(&svc("data", "/data:/etc"), "env-1", &ctx).is_err());
  }

  #[test]
  fn a_user_pod_cannot_reach_the_api_server_or_escalate() {
      let p = workspace_pod(&ws_spec(), "ws-1", &ctx());
      let s = p.spec.unwrap();
      assert_eq!(s.automount_service_account_token, Some(false));
      assert_eq!(s.node_name.as_deref(), Some("session-0"));
      let c = &s.containers[0];
      let sc = c.security_context.as_ref().unwrap();
      assert_eq!(sc.allow_privilege_escalation, Some(false));
      assert_eq!(sc.capabilities.as_ref().unwrap().drop.as_deref(), Some(&["ALL".to_string()][..]));
      // Requests AND limits, both, on every user container: requests are what the scheduler packs
      // against, limits are what stops one workspace eating a 32-OCPU node.
      let r = c.resources.as_ref().unwrap();
      assert!(r.requests.as_ref().unwrap().contains_key("memory") && r.limits.as_ref().unwrap().contains_key("memory"));
  }
  ```
  Command: `cargo test -p kloudlite-workspaces k8s::`
  Expected failure: `could not find 'k8s' in the crate root`.
- [ ] **Step 2:** Implement `k8s.rs`. Load-bearing details, each one a real bug if missed:
  - The hostPath source is `format!("{}/vol/{}/live/volumes/{}", pool, env_id, m.folder)` **after**
    `validate_mount(m)?` — built from validated segments, never concatenated from a caller string.
  - The workspace pod keeps `container.rs`'s double mount (`/workspace` RW plus
    `/usr/share/nginx/html` RO) so `default_ws_image()`'s `nginx:alpine` still serves the
    workspace's own files with zero configuration. Carry `container.rs`'s comment across.
  - `spec.nodeName` is set directly (not `nodeAffinity`): the node is already decided, and naming
    it is both simpler and what makes an unschedulable pod fail visibly instead of silently
    pending on an unsatisfiable expression. Keep the `nodeSelector` on `kloudlite.io/role` and the
    matching `toleration` — the label without the taint toleration schedules nothing.
  - `restart_policy: Always` on the workspace Pod is what `--restart unless-stopped` became.
  - Every child object carries `owner_references: vec![owner_ref]` with `controller: Some(true)`
    so deletion cascades via garbage collection.
  - Defaults from the spec: workspace request `500m`/`1Gi`, limit `4`/`8Gi`; service request
    `250m`/`512Mi`, limit `2`/`4Gi`.
  - `image_pull_policy` left unset — the kubelet's `IfNotPresent` default for a tagged image is
    already what we want, and pinning it would break `:latest`.
- [ ] **Step 3:** `cargo test -p kloudlite-workspaces k8s:: && cargo clippy --workspace -- -D warnings && cargo test`
  Expected: new tests pass; `compose.rs`'s original test still passes (it has not been deleted yet
  — deliberate duplication for exactly one commit's worth of overlap).
  Commit: `Build namespaces, pods, deployments and policies from the workspace domain types`

---

### Task 4: Placement against `OwnerBinding` and node allocatable

**Files:** `crates/workspaces/src/placement.rs`, `crates/workspaces/src/lib.rs`.

**Interfaces:**

```rust
// crates/workspaces/src/placement.rs
/// The node this owner's data lives on, creating the binding on their first object in the region.
/// `Ok(None)` means no candidate node exists yet — the caller answers 503 and the user retries;
/// it never picks a node it cannot verify.
pub async fn place(client: &kube::Client, region: &str, owner: &str, role: &str)
    -> Result<Option<String>, kube::Error>;

/// Node allocatable minus the sum of scheduled pod requests, per candidate node. Real numbers from
/// the API, replacing `scheduler.rs`'s `capacity - used` guess over a flat JOB_CPU/JOB_MEM_MB
/// estimate (already marked `ponytail:` as not real accounting).
async fn free_mem_bytes(client: &kube::Client, node: &str) -> Result<i64, kube::Error>;
```

- [ ] **Step 1:** Failing test — port `scheduler.rs`'s five behavioural tests to a mocked client
  (see Task 5 Step 1 for the mock harness; write the harness here, in
  `crates/workspaces/src/kube_test.rs` behind `#[cfg(test)]`, and reuse it from Task 5/6):
  ```rust
  /// `kube::Client::new` takes any `tower::Service<http::Request<Body>>` (kube-client 4.2.0,
  /// client/mod.rs:153), which is the whole mock: a `service_fn` matching on method+path and
  /// answering canned JSON. Chosen over an envtest-style real apiserver because Rust has no
  /// envtest and downloading a kube-apiserver binary in `cargo test` is a non-starter; a real
  /// API server IS tested — by tests/ws_e2e.sh, against real k3s.
  pub fn mock_client(routes: Vec<(&'static str, &'static str, serde_json::Value)>) -> kube::Client;
  ```
  Tests to port, name for name, because each encodes a decision `scheduler.rs` got right:
  `first_object_creates_binding_to_sole_node`, `second_owner_may_share_the_same_node`,
  `owner_pins_to_their_binding_regardless_of_later_load_changes`,
  `concurrent_first_objects_for_one_owner_converge_on_one_binding` (the 409-adopt path),
  `dead_bound_node_leaves_placement_pinned_without_rehoming`.
  Command: `cargo test -p kloudlite-workspaces placement::`
  Expected failure: module not found.
- [ ] **Step 2:** Implement. `bind_owner`'s conflict-adopt logic carries over **verbatim** in shape;
  only the error type changes:
  ```rust
  match api.create(&PostParams::default(), &binding).await {
      Ok(_) => Ok(Some(node)),
      // Someone raced us to the first binding for this owner — adopt theirs rather than erroring,
      // so both callers converge on the same node. This is `StoreErr::Conflict` under a different
      // name, and `resourceVersion` gives it the same optimistic-concurrency guarantee Cosmos did.
      Err(kube::Error::Api(ae)) if ae.code == 409 => match api.get_opt(&name).await? {
          Some(b) => Ok(Some(b.spec.node_name)),
          None => Ok(Some(node)), // vanished between the conflict and the re-read; retry-safe
      },
      Err(e) => Err(e),
  }
  ```
  The dead-node rule survives unchanged and deliberately: a bound node that is gone or NotReady
  still returns its `node_name`. Re-homing an owner is a migration of their subvolumes, not a
  scheduling decision — under k8s it surfaces as a pod stuck `Pending`, which is visible, unlike a
  job sitting `Queued` forever. Carry `scheduler.rs:91`'s comment across word for word.
  `binding_name` collision guard: if the existing binding's `spec.owner != owner` (two owner slugs
  differing only in case flattening to one RFC-1123 name), return an error rather than adopting
  someone else's node. `// ponytail: case-flattened binding names; a hash suffix if slugs ever
  collide for real.`
- [ ] **Step 3:** `cargo test -p kloudlite-workspaces placement:: && cargo clippy --workspace -- -D warnings && cargo test`
  Commit: `Place owners on nodes with an OwnerBinding CRD and real node allocatable`

---

### Task 5: `/v1` writes CRDs

The commit where the API stops producing jobs. The job machinery is still compiled and still
tested after this commit — it is simply no longer reachable from `/v1`. That is the deliberate
"both paths exist" window, and it is exactly one commit wide (Task 7 closes it). It is also the
point where the system is functionally split: nothing consumes CRDs until Task 6. **Do not deploy
between Task 5 and Task 7.**

**Files:** `crates/workspaces/src/api.rs`, `bins/api/src/main.rs`,
`crates/workspaces/tests/{api_user,api_teams,api_volumes}.rs`.

**Interfaces:**

```rust
pub struct ApiState {
    /// Regions only now — every workspace/environment fact lives in the cluster.
    pub store: Arc<dyn MetaStore>,
    pub jwt: Arc<Jwt>,
    pub admins: HashSet<String>,
    pub registry: Option<RegistryClient>,
    pub membership: Option<Arc<dyn MembershipCheck>>,
    /// `None` when no kubeconfig/in-cluster config is available: every workspace and environment
    /// route answers 503 rather than not existing, the same shape `registry: None` already has.
    pub kube: Option<kube::Client>,
}
impl ApiState { pub fn with_kube(self, client: kube::Client) -> Self; }
```

Route table is unchanged. Every handler's body changes.

**Callers of the changed constructor:** `bins/api/src/main.rs:112`,
`crates/workspaces/tests/api_user.rs`, `api_teams.rs`, `api_volumes.rs` (each builds an `ApiState`
in a local helper — grep `ApiState::new` before starting; there are no others).

- [ ] **Step 1:** Failing tests. Rewrite `crates/workspaces/tests/api_user.rs`'s assertions: every
  `s.store.queued_jobs("centralindia")` (lines 89, 131, 188, 224, 291, 368, 436, 458, 480) and
  `api_teams.rs:177` becomes an assertion about the CRD the handler POSTed to the mock client. E.g.
  ```rust
  /// `create_ws` writes a Volume and a Workspace, both stamped with the owner's bound node, and
  /// answers 202 with the object it wrote. Two objects, not one, because a workspace and an
  /// environment own a btrfs subvolume with identical semantics and one Volume controller ends
  /// the branching the job system needed.
  #[tokio::test]
  async fn create_ws_writes_a_volume_and_a_workspace_pinned_to_the_owner_s_node() { … }

  /// The invariant that, violated, splits an owner's data across pools (audit H1): the Workspace's
  /// nodeName is READ from the Volume it references, never chosen a second time.
  #[tokio::test]
  async fn a_workspace_never_names_a_node_its_volume_does_not() { … }
  ```
  Command: `cargo test -p kloudlite-workspaces --test api_user`
  Expected failure: `no method named 'queued_jobs'` is *not* what you should see yet — at this
  point it still exists, so the failure is the new assertions failing on 202-with-no-CRD-written.
- [ ] **Step 2:** Replace `ws_job`/`env_job`/`push_job` with CRD writes. The mapping, handler by
  handler — this is the whole task and there are no other semantics to invent:

  | Route | Was | Becomes |
  | --- | --- | --- |
  | `POST /v1/workspaces` | doc + `WsCreate` job | `place()`, then create `Volume{source: None}` + `Workspace{desiredState: Running}` |
  | `POST /v1/workspaces/{id}/start` \| `/stop` | `WsStart`/`WsStop` job | `patch` `spec.desiredState` = Running/Stopped |
  | `POST /v1/workspaces/{id}/push` | `Push` job | `patch` an annotation `kloudlite.io/push-requested: {rfc3339}` on the `Volume` — a spec-level generation bump the controller converges toward, keeping "push is the one mutating verb" while the *object* stays the work item |
  | `POST /v1/workspaces/{id}/clone` | `WsClone` job | new `Volume{source: CloneOf{volume: src}}` + `Workspace` |
  | `POST /v1/workspaces/restore` | `WsRestore` job | new `Volume{source: RestoreOf{volume, snapshotId}}` + `Workspace` |
  | `DELETE /v1/workspaces/{id}` | doc + `WsDelete` job | `api.delete()` the `Workspace`, then the `Volume`; the finalizer orders subvolume removal after container removal |
  | `POST /v1/environments` | doc + `EnvUp` job | `check_mounts` (unchanged), `place()`, `Volume` + `Environment{desiredState: Running}` |
  | `/environments/{id}/start` \| `/stop` | `EnvUp`/`EnvDown` | `patch` `spec.desiredState` |
  | `/environments/{id}/clone` | `WsClone` job w/ `stop_project` | `Volume{source: CloneOf}` + `Environment`; `stop_project` and `crate::engine::compose::project` disappear from this file |
  | `DELETE /v1/environments/{id}` | `EnvDelete` job | `api.delete()` both |
  | `GET` list/get (`list_ws`, `get_ws`, `list_env`, `get_env`, `list_volumes`) | store read | `api.list(&ListParams::default().labels(&format!("kloudlite.io/owner={owner}")))`, projecting `spec` + `status` into the same JSON body the web app already parses |

  The list filter is a **label**, not a field selector: `metadata.labels` is indexed for label
  selectors by every API server, while an arbitrary spec field needs a `selectableFields` entry
  (we have one, for `nodeName`, and adding one per query axis is how a CRD becomes a database).
  Stamp `kloudlite.io/owner` and `kloudlite.io/kind` on every object at create time.

  `state`/`WsState`/`EnvState` in the response body come from `status.phase`, defaulting to
  `creating` when `status` is absent — a just-created object has no status until the controller
  writes one, and a `null` there would break the web app's enum.
  `list_ws`'s "deleted docs are filtered out" rule disappears: a deleted object is gone from the
  API server. Delete the filter and the comment together — the reason it existed is gone.
- [ ] **Step 3:** Wire `bins/api/src/main.rs`: build the client once,
  ```rust
  // In-cluster config when the pod has a ServiceAccount, else the operator's kubeconfig. `None`
  // is a legitimate dev configuration (no cluster) — workspace routes answer 503, the same shape
  // KLOUDLITE_VOL_AGENT_URL being unset already has.
  match kube::Client::try_default().await {
      Ok(c) => state = state.with_kube(c),
      Err(e) => eprintln!("no kubernetes config ({e}): /v1 workspace routes will answer 503"), // ponytail: eprintln
  }
  ```
  The Cosmos store construction stays exactly as it is — `Region` still lives there, and it is
  cross-cluster metadata that cannot live in any one cluster's API server.
- [ ] **Step 4:** `cargo test -p kloudlite-workspaces && cargo clippy --workspace -- -D warnings && cargo test`
  Expected: rewritten API tests pass; `scheduler.rs`'s and `lease.rs`'s own tests still pass
  (unreached but not yet deleted).
  Commit: `Write workspace and environment CRDs from the /v1 API`

---

### Task 6: The agent becomes a node-level controller

**Files:** `bins/agent/Cargo.toml`, `bins/agent/src/controller.rs`, `bins/agent/src/lib.rs`,
`bins/agent/src/main.rs`, `bins/agent/tests/reconcile.rs`; **deletes**
`bins/agent/src/container.rs`, `bins/agent/tests/loop.rs`.

**Interfaces:**

```rust
// bins/agent/src/controller.rs
pub struct Ctx {
    pub client: kube::Client,
    pub engine: Arc<Engine>,
    pub node: String,
    pub pool: String,
    /// In-flight long btrfs operations keyed by `{uid, generation}`. THE idempotency guard, and a
    /// local in-memory check rather than a distributed lease because the field selector already
    /// guarantees this node is the only reconciler of this object.
    pub running: Mutex<HashMap<(String, i64), JoinHandle<Result<Done, String>>>>,
}

/// Runs all three controllers to completion (i.e. forever). Returns only on shutdown signal.
pub async fn run(ctx: Arc<Ctx>) -> Result<(), String>;

async fn reconcile_volume(v: Arc<crd::Volume>, ctx: Arc<Ctx>) -> Result<Action, ReconcileErr>;
async fn reconcile_workspace(w: Arc<crd::Workspace>, ctx: Arc<Ctx>) -> Result<Action, ReconcileErr>;
async fn reconcile_environment(e: Arc<crd::Environment>, ctx: Arc<Ctx>) -> Result<Action, ReconcileErr>;
fn error_policy<K>(obj: Arc<K>, err: &ReconcileErr, ctx: Arc<Ctx>) -> Action;
```

`bins/agent/src/lib.rs` keeps `Config`, `blob_store`, `build_engine`, `meta_store_from_env`,
`owner_file`, `record_owner`, `spawn_janitor`, `janitor_volume_snapshots`, `janitor_sweep_stage`,
`cleanup_local`, `other_unpushed_blobs`, `other_lineage_snap_names`, `btrfs_delete` and their tests
unchanged. `Config` gains `node: String` (from `NODE_NAME`) and loses `cpu`/`mem_mb`/`disk_gb`
(node allocatable replaces them) — **callers:** `bins/agent/src/main.rs:20,37` and
`bins/agent/src/lib.rs`'s `run`.

- [ ] **Step 1:** Failing test in `bins/agent/tests/reconcile.rs`, using the same mocked client:
  ```rust
  /// The single-flight guard: a second reconcile of the same {uid, generation} while a push is
  /// running must NOT start a second one. This replaces the 120s-lease-with-no-renewal that audit
  /// H2 is about — the sweep requeuing a still-running job and it racing itself.
  #[tokio::test]
  async fn a_second_reconcile_of_a_running_generation_does_not_start_a_second_operation() { … }

  /// A finished operation is observed on a LATER pass and written to status, and the reconcile
  /// that observes it requeues no further.
  #[tokio::test]
  async fn a_finished_operation_writes_observed_generation_and_stops_requeueing() { … }

  /// Keep-biased: an API error or an unreadable pool means requeue with backoff, never "reality
  /// doesn't match, so remove it". Same discipline as crates/registry/src/gc.rs.
  #[tokio::test]
  async fn a_reconcile_that_cannot_read_the_pool_deletes_nothing() { … }
  ```
  Command: `cargo test -p kloudlite-agent-bin --test reconcile`
  Expected failure: `unresolved import kloudlite_agent::controller`.
- [ ] **Step 2:** Write `controller.rs`. The exact kube-rs surface, all verified against 4.2.0:
  ```rust
  use kube::runtime::controller::{Action, Controller};
  use kube::runtime::finalizer::{finalizer, Event as FinalizerEvent};
  use kube::runtime::watcher;
  use kube::{Api, api::{Patch, PatchParams}};

  pub async fn run(ctx: Arc<Ctx>) -> Result<(), String> {
      // `spec.nodeName` is the whole sharding story: two nodes cannot contend for one object
      // because the object names its node. There is no acquisition, no expiry, no requeue sweep.
      // NB the RBAC grant is cluster-wide — a field selector narrows a watch, never authorization.
      let mine = watcher::Config::default().fields(&format!("spec.nodeName={}", ctx.node));
      let vols: Api<crd::Volume> = Api::all(ctx.client.clone());   // cluster-scoped CRDs
      let pods: Api<Pod> = Api::all(ctx.client.clone());

      let volumes = Controller::new(vols, mine.clone())
          .shutdown_on_signal()
          .run(reconcile_volume, error_policy, ctx.clone())
          .for_each(|r| async move { if let Err(e) = r { eprintln!("volume reconcile: {e}") } });
      let workspaces = Controller::new(Api::<crd::Workspace>::all(ctx.client.clone()), mine.clone())
          .owns(pods, watcher::Config::default())
          .shutdown_on_signal()
          .run(reconcile_workspace, error_policy, ctx.clone())
          .for_each(|r| async move { if let Err(e) = r { eprintln!("workspace reconcile: {e}") } });
      // …environments, .owns(Api::<Deployment>::all(..))…
      tokio::join!(volumes, workspaces, environments);
      Ok(())
  }
  ```
  `Controller::new(main_api: Api<K>, wc: watcher::Config)` (kube-runtime `controller/mod.rs:724`),
  `.owns::<Child>(api, wc)` (`:992`), `.shutdown_on_signal()` (`:1639`),
  `.run(reconciler, error_policy, context)` returning a `Stream<Item = Result<(ObjectRef<K>, Action), _>>`
  (`:1683`). `watcher::Config::fields(&str)` (`watcher.rs:308`) sets `field_selector`.
  `Action::requeue(Duration)` / `Action::await_change()` (`controller/mod.rs:100,113`).

  `reconcile_volume`'s body is exactly the spec's three steps:
  ```rust
  finalizer(&api, crd::SUBVOLUME_FINALIZER, v.clone(), |event| async {
      match event {
          // Deleting a Volume blocks until cleanup_local has run: containers gone first (GC via
          // ownerReferences), then the subvolume, then the object disappears. That ordering is
          // what makes audit H5 (a deleted workspace resurrected by an in-flight job)
          // unexpressible rather than patched.
          FinalizerEvent::Cleanup(v) => cleanup_volume(&v, &ctx).await,
          FinalizerEvent::Apply(v) => apply_volume(&v, &ctx).await,
      }
  }).await
  ```
  and `apply_volume`:
  1. `status.observed_generation == Some(meta.generation)` and no pending push annotation →
     `Ok(Action::await_change())`.
  2. Key `(uid, generation)` already in `ctx.running` → if finished, drain it into status and
     return `await_change()`; else set `Progressing=True`, `Ok(Action::requeue(15s))`.
  3. Otherwise `tokio::task::spawn_blocking(move || { … })` — **not** `tokio::spawn`, and for the
     same reason `run_job_blocking` did it: `engine::ws_lock`'s synchronous `libc::flock` must not
     sit on the reactor. Carry `lib.rs`'s module-doc paragraph across verbatim; it is still true.
     Insert into the map, set `Progressing=True`, `Ok(Action::requeue(15s))`.

  Status writes are server-side apply on the subresource, which requires apiVersion/kind in the
  patch body:
  ```rust
  // patch_status (kube-client api/subresource.rs:494). Apply, not Merge: the field manager owns
  // exactly the status fields it sets, so two writers cannot silently clobber each other.
  api.patch_status(&name, &PatchParams::apply(crd::AGENT_FIELD_MANAGER).force(),
      &Patch::Apply(serde_json::json!({
          "apiVersion": "kloudlite.io/v1alpha1", "kind": "Volume",
          "status": { "phase": phase, "observedGeneration": gen, "conditions": conds }
      }))).await?;
  ```
  Guard it: skip the write when the computed status equals the observed one. A status write that
  triggers its own watch event and reconciles again is the classic controller hot loop, and it is
  an outage, not a warning. (This is what the `derive = "PartialEq"` in Task 1 is for.)

  `reconcile_workspace`: ensure `Namespace`, ensure `default_policies`, then
  `desiredState == Running` → server-side-apply the pod from `k8s::workspace_pod`;
  `Stopped` → delete it, tolerating 404. `pod_ref` and `phase` into status. **The pod's node is
  read from the referenced `Volume`, never recomputed** — refuse (`Degraded=True`, no pod) if they
  disagree, which is the one invariant that splits an owner's data across pools when violated.
  `reconcile_environment`: same, over `service_deployment` + `service_clusterip` per `Service`.
  The `EnvDown` "always push before down" rule survives as: a `desiredState: Stopped` reconcile
  first requests a push on the `Volume` (annotation) and waits for `observedGeneration` to catch up
  before deleting the Deployments. An env that stops without pushing loses its last state for good.
- [ ] **Step 3:** `main.rs` and `lib.rs`. Delete `container.rs`, `register`, `agent_id_path`,
  `run_with_engine`, `run_job`, `run_job_blocking`, `report`, `ws_doc`, `ws_from_payload`,
  `env_owner_id`, `env_dir`, `docker_stop_name`, `docker_start_name`, `compose`, `JOB_CPU`,
  `JOB_MEM_MB`, `pub mod container;`. `mkdir_env_mounts` **survives** and moves into
  `controller.rs` unchanged — every declared folder must exist before a hostPath binds it, and its
  `validate_mount` call is a security check, not a formality. `run` becomes:
  ```rust
  pub async fn run(cfg: Config) -> Result<(), String> {
      let meta = meta_store_from_env().await?;
      let engine = Arc::new(build_engine(&cfg.pool, meta, &cfg.api_url, &cfg.agent_token));
      spawn_janitor(engine.clone(), cfg.pool.clone());
      // The CRDs must be Established before the watch starts, or it fails at startup and the
      // controller sits idle looking healthy — Phase 2A Task 8 Step 5c proves this ordering once
      // deliberately. Fail loudly here rather than reproducing it in production.
      let client = kube::Client::try_default().await.map_err(|e| e.to_string())?;
      controller::run(Arc::new(controller::Ctx { … })).await
  }
  ```
  `WS_REGISTRY_URL` and `WS_AGENT_TOKEN` are still read: the Engine's `RegistryClient` pushes
  commit records and moves `vol/{owner}/{id}` refs there, and that surface is unchanged.
  Also write the DaemonSet's heartbeat file (`{pool}/.agent-heartbeat`) on each successful
  reconcile-loop tick — Phase 2A Task 8 Step 2 ships that liveness probe commented out waiting on
  exactly this; uncomment it there in the same commit.
  Delete `bins/agent/tests/loop.rs` in this commit: it drives `/vol-agent/register|work|jobs/*`,
  which the next task removes and which nothing can replace.
- [ ] **Step 4:** `cargo test -p kloudlite-agent-bin && cargo clippy --workspace -- -D warnings && cargo test`
  Expected: `reconcile.rs` passes; the janitor tests still pass; `loop.rs` is gone from the test
  listing.
  Commit: `Rewrite the agent as a node-scoped Kubernetes controller`

---

### Task 7: Delete the job system

Nothing reaches it any more. This is a pure deletion commit — if anything here needs a behaviour
decision, a previous task was left unfinished.

**Files:** `crates/workspaces/src/model.rs`, `store.rs`, `cosmos.rs`, `lib.rs`;
**deletes** `crates/workspaces/src/{lease.rs,scheduler.rs,engine/compose.rs}`;
`crates/workspaces/src/engine/mod.rs`; `bins/server/src/{vol_agent.rs,boot.rs,router/mod.rs}`;
`tests/{vol_agent.rs,common/mod.rs}`; root `Cargo.toml`; `crates/workspaces/Cargo.toml`.

**Every call site, enumerated** (grep `JobKind|JobState|AgentDoc|Capacity|Binding|create_job|queued_jobs|leased_jobs|replace_job|get_job|agents_in|upsert_agent|get_binding|create_binding|spawn_sweep|vol_agent_job_routes|JobsState|lease::|scheduler::|compose::|container::` and expect zero hits afterwards):

| File | What goes |
| --- | --- |
| `crates/workspaces/src/model.rs` | `Capacity` (8-12), `AgentDoc` (27-40), `Binding` (46-51), `JobKind` (258-290), `JobState` (292-299), `Job` (301-312) |
| `crates/workspaces/src/store.rs` | `MetaStore::{upsert_agent, agents_in, get_binding, create_binding, create_job, queued_jobs, leased_jobs, get_job, replace_job}`; `MemStore`'s `agents`/`jobs`/`bindings` fields and impls |
| `crates/workspaces/src/cosmos.rs` | the same nine impls; the `agents`/`jobs`/`bindings` container handles |
| `crates/workspaces/src/lease.rs` | whole file |
| `crates/workspaces/src/scheduler.rs` | whole file (its owner-binding logic lives in `placement.rs` since Task 4) |
| `crates/workspaces/src/engine/compose.rs` | whole file; drop `pub mod compose;` from `engine/mod.rs` |
| `crates/workspaces/src/lib.rs` | `pub mod lease;`, `pub mod scheduler;` |
| `bins/server/src/vol_agent.rs` | `register`, `work`, `job_done`, `job_failed`, `spawn_sweep`, `vol_agent_job_routes`, `RegionHint`, `region_by_token`, `mark_ws_ready`/`mark_ws_stopped`/`mark_ws_error`/`mark_env_state`/`mark_env_error`, `job_store_err`, `job_not_found`, `JobsState::{store, poll_window, poll_interval}`. **Keep** `VOL_AGENT_TAILS`, `vol_agent_route`, `vol_agent_prefixed`, `commits`, `move_ref`, `history`, `authorized`, and both existing tests. |
| `bins/server/src/boot.rs:19-42` | `build_jobs_state` stops constructing a Cosmos store and stops calling `spawn_sweep`; `JobsState::new()` takes no argument |
| `bins/server/src/router/mod.rs:30` | drop `.merge(crate::vol_agent::vol_agent_job_routes())`; line 58's `Extension(JobsState::new(None))` becomes `JobsState::new()` |
| `tests/common/mod.rs:114-130` | delete `jobs_state_with_store`, keep `no_jobs_state` |
| `tests/vol_agent.rs` | delete the register/work/done/failed tests; keep the commits/ref/history ones. Read the file first and split by which route each test drives. |
| `crates/workspaces/tests/engine_ops.rs:56` | `JobsState::new(None)` → `JobsState::new()` |
| `crates/workspaces/Cargo.toml` | drop `serde_yaml` |
| root `Cargo.toml` | drop `serde_yaml` from `[workspace.dependencies]` **only if** nothing else uses it |

- [ ] **Step 1:** Prove `serde_yaml`'s last user is going away, before claiming it:
  `grep -rn 'serde_yaml' --include='*.rs' --include='*.toml' . | grep -v '^\./target'`
  Expected after deleting `compose.rs`: only the two `Cargo.toml` declarations. If any other hit
  exists, leave the dependency and note it — the spec's "check before claiming it" is exactly this.
- [ ] **Step 2:** Delete in the order of the table (leaves first: `lease.rs`, `scheduler.rs`,
  `compose.rs`, then the server routes, then the store trait, then the model types). Compile
  between each: `cargo check --workspace` is the fastest way to find a call site the table missed.
- [ ] **Step 3:** `cargo build --workspace && cargo clippy --workspace -- -D warnings && cargo test`
  Expected: clean. Then the negative check:
  `grep -rn 'JobKind\|JobState\|AgentDoc\|lease::sweep\|spawn_sweep\|vol-agent/work' --include='*.rs' . | grep -v '^\./target' | wc -l` → `0`.
  And the routing contract still holds: `cargo test --test routing` passes untouched — the record
  routes never moved.
- [ ] **Step 4:** Commit: `Delete the volume job queue, lease sweep and agent registration`
  Body should name the four audit findings that cease to be expressible (H1, H4, H7, P6) and the
  two retired costs (P5's heartbeat ops, P7).

---

### Task 8: Rewrite `tests/ws_e2e.sh` for the cluster

**Files:** `tests/ws_e2e.sh`, `CLAUDE.md`.

The btrfs half — push, clone, restore, the MongoDB clone-fidelity check — is unchanged, because
that engine is unchanged. That the test's hardest assertions do not move is the signal this
migration was scoped correctly. Four things change, plus three new assertions.

- [ ] **Step 1:** Prerequisites. Add to the existing 77-skip block, keeping its shape:
  ```sh
  kubectl version --request-timeout=5s >/dev/null 2>&1 || { echo "SKIP: no reachable kubernetes cluster" >&2; exit 77; }
  kubectl get crd volumes.kloudlite.io >/dev/null 2>&1 || { echo "SKIP: kloudlite CRDs not installed (deploy/k3s/crds.yaml)" >&2; exit 77; }
  ```
  A single-node k3s in the CI VM is enough — one node carrying both role labels with taints
  relaxed. Nothing about this test's value depends on there being two nodes.
  Delete the `docker compose version` prerequisite; keep `btrfs`/`sudo`/Cosmos/Azure.
  Verify: `bash -n tests/ws_e2e.sh && ./tests/ws_e2e.sh; echo $?` on this Mac → `77`.
- [ ] **Step 2:** Waiting changes shape, for the better. Replace every "poll a document until its
  state string flips" loop with the condition the controller writes:
  ```sh
  kubectl wait --for=condition=Ready workspace/"$WS_ID" --timeout=300s
  ```
  This asserts the *contract* (`Ready` means ready) rather than a state string, and it fails with
  the condition's own message instead of a timeout with no reason.
- [ ] **Step 3:** Container assertions move from `docker` to `kubectl`:
  `sudo docker exec env-{id}-db-1 mongosh …` → `kubectl -n env-{id} exec deploy/db -- mongosh …`.
  `grep -n 'docker exec\|docker compose\|docker ps' tests/ws_e2e.sh` → `0` when done. Grep the
  docs for the same commands and fix every runbook line in the same commit.
- [ ] **Step 4:** Add the three assertions that are what this design actually claims, none of which
  anything else in the suite covers:
  ```sh
  # 1. Service-to-service DNS across namespaces — the thing compose genuinely provided and the
  #    reason the replaced design was going to hand-roll a DNS resolver.
  kubectl -n "ws-$WS_ID" exec ws -- getent hosts "db.env-$ENV_ID" | grep -q . || fail "cross-namespace DNS"

  # 2. A default-deny namespace actually denies. A NetworkPolicy nobody tests is a NetworkPolicy
  #    that is silently not enforced — kube-router not being enabled is a config typo away and
  #    produces no error, only permitted traffic. Prove the negative.
  kubectl -n "$UNATTACHED_NS" exec probe -- timeout 5 wget -qO- "db.env-$ENV_ID:27017" && fail "default-deny is not enforced"

  # 3. Reconcile converges. Delete a Deployment out from under a running environment and assert it
  #    comes back with nobody calling the API. That is the whole claim of moving to controllers.
  kubectl -n "env-$ENV_ID" delete deploy db
  kubectl -n "env-$ENV_ID" rollout status deploy/db --timeout=120s || fail "controller did not converge"
  ```
- [ ] **Step 5:** Update `CLAUDE.md`'s `./tests/ws_e2e.sh` line: the prerequisite is now a k3s
  cluster plus root-capable btrfs, and the three binaries are unchanged (the agent is now a
  controller, not a poller). One sentence, in the existing comment block's voice.
- [ ] **Step 6:** Run it for real on the btrfs + k3s VM: `./tests/ws_e2e.sh; echo $?` → `0`.
  Exit 77 does not count as a pass.
  Commit: `Rewrite the workspaces e2e test against a k3s cluster`

---

## What is deliberately not in this plan

- **A cluster-wide controller or `coordination.k8s.io` Leases.** Sharding by `spec.nodeName` gives
  one candidate per shard; a Lease would guard nothing. Note it where the admission-side placement
  decider would move if it ever leaves the API.
- **A ValidatingAdmissionWebhook for `validate_mount`.** The spec wants it and it is strictly
  stronger, but it needs a serving cert, a Service, and a failure policy — a deployment surface, not
  a code change, and `validate_mount` still runs on both the `/v1` path and the pod-building path
  meanwhile. `// ponytail:` it in `k8s.rs`.
- **A cross-region projection store.** Build it when a cross-region listing is asked for, and build
  it write-only.
- **Resumable mid-`send` reconcile.** A restarted operation is a retry, not a corruption.
- **Migrating live data.** The spec's five-step migration (push everything, then translate docs to
  CRDs with a one-shot script) is an operational runbook, not a code task. It runs after Task 8.
