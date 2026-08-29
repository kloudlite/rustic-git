# Persistent per-person home Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Everything under `/home/kl` except `~/workspace` is one btrfs subvolume per person per node (`home-{owner}`), mounted into every workspace pod, replicated to the region's registry on a timer and on every workspace stop, and pulled back on a node that has never seen it.

**Architecture:** No new CRD and no new `/v1` route: the `OwnerBinding` reconciler (`bins/agent/src/binding.rs`) authors one child `Volume` named `home-{owner}` plus a local PV and a `home` PVC in each of the owner's namespaces, exactly the way parents author their volume children today, and the existing Volume controller materializes it (pulling the registry's `main` ref when this node has no subvolume and history exists). Caches are nested subvolumes (`k8s::HOME_LOCAL_DIRS`) that `btrfs send` and the qgroup both ignore. Replication reuses `Engine::push_env`: an agent beat every `WS_HOME_PUSH_SECS` pushes homes whose btrfs generation moved, and `apply_workspace`'s `Stopped` arm gates the pod delete on a `stop-home-{ws}` `SnapshotRequest` landing, the same fail-closed shape `apply_environment` already uses.

**Tech Stack:** Rust (kube 0.x runtime controllers, k8s-openapi, tokio, serde/schemars), btrfs (`subvolume create/show`, nested subvolumes, qgroups), the existing `Engine` (`crates/workspaces/src/engine`), the fake-kube harness (`rustic_git_workspaces::kube_test`), bash e2e against k3s.

**Spec:** `docs/superpowers/specs/2026-08-29-workspace-home-design.md`

## Global Constraints

- **Single writer per subvolume.** Inside a region one node per person (the `OwnerBinding`), so two nodes never push the same home; the timer and the stop push serialise on the volume's `ws_lock` flock inside the engine.
- **Keep-biased.** Nothing is created empty and later overwritten, nothing is deleted to converge; an unreachable registry at first materialization is `phase: Error`, reason `RegionUnreachable`, and no subvolume exists afterwards. Nested cache subvolumes that already exist (or exist as plain directories the person made) are left alone.
- **Fail-closed stop.** A workspace stop deletes its pod only after `stop-home-{ws}` reaches `done`; a failed push leaves the workspace at its current phase with `Ready=False` reason `StopSnapshotFailed`, never tears down. The request is deleted after the teardown, like `stop-{env}`.
- **Labels are views.** `spec.owner` on the home Volume is the truth; `rustic-git.io/owner`, `/kind`, `/team` and the `rustic-git.io/stop-of` label on the stop request exist for listing and watching only. Never authorize on a label.
- **The agent may not write spec** except the documented exceptions. Today: `Volume.spec.restoreTo` (`restore_gate`). This plan adds ONE more: `Volume.spec.quotaGb` on a Volume owned by an `OwnerBinding` (Task 8), allowed by name in `deploy/k3s/agent-admission.yaml`. The home Volume is CREATED by the agent's binding reconciler through `ensure_child_volume`, exactly like a Workspace's child Volume is today — `create` is not governed by the spec-is-read-only policy (it matches `UPDATE` only) and is already granted in RBAC.
- **Any new agent kube write is a row in the RBAC table in `deploy/k3s/agent-rbac.yaml` (the table IS the role) AND must be allowed by the admission policies in `deploy/k3s/agent-admission.yaml`.** Every write in this plan reuses a verb already granted (`volumes: create,patch`; `snapshotrequests: create,delete`; `persistentvolumes`/`persistentvolumeclaims: create,patch`); the table's call-site column is updated in Tasks 2, 7 and 8.
- **No `hostPath`.** The home reaches the pod as a statically provisioned `local` PV with node affinity (`k8s::local_pv`), like the subvolume and the Nix store; `no_pod_this_module_builds_uses_a_hostpath` in `k8s.rs` stays green.
- **CRD manifest regenerated** after every spec-struct change: `CRD_REGEN=1 cargo test -p rustic-git-workspaces --test crd_yaml`, and `deploy/k3s/crds.yaml` committed in the same commit.
- **Clippy clean:** `cargo clippy --workspace --all-targets -- -D warnings` introduces no NEW warnings in files you touch (CI gates `--workspace -- -D warnings`).
- **Comments explain WHY, never what**; match the density of `bins/server/src/router/route.rs`. Deliberate ceilings are marked `// ponytail: <ceiling and upgrade path>`.
- **Commit subjects are imperative sentence case with no tool attribution.** No `Co-Authored-By`, no "Generated with".
- Paths and names fixed by the spec: subvolume `{pool}/vol/home-{owner}/live`; registry name `vol/{owner}/home-{owner}`; mount `/home/kl` (`k8s::HOME_DIR`) with `k8s::WORKSPACE_DIR = /home/kl/workspace` inside it; PVC `home`; nested dirs `.cache`, `.npm`, `.cargo/registry`, `.local/share/pnpm`; generation file `{pool}/vol/home-{owner}/.pushed-gen`; env `WS_HOME_PUSH_SECS` default `300`; timer commit message `home: periodic`; `homeQuotaGb` default `2`; stop request `stop-home-{ws}`.
- **One deviation from the spec, stated here so nobody hunts for it:** the spec names the PV `home-{owner}`. A local PV binds to exactly one claim, and an owner with workspaces in two teams has two namespaces (`ws-{owner}`, `wt-{owner}-{tail}`) that each need a `home` claim on the same host path — so the PV is per NAMESPACE, `home-{ns}` (`k8s::home_pv_name`). The Volume, subvolume and registry name stay per owner as specified.
- **Spec item not implemented, by decision (see "Not a task" after Task 8):** "the workspace list shows the home's usage next to the workspace's own (`status.usageBytes` already exists on `Volume`)". It does not exist — `usageBytes`/`usage_bytes` appears nowhere in `crates/`, `bins/` or `web/` — and no usage is shown for workspaces' own volumes either. Building it is a new feature (qgroup read → `VolumeStatus` → `/v1` projection → web) for both kinds and gets its own spec.

---

## File map

| File | Responsibility in this plan |
|---|---|
| `crates/workspaces/src/crd.rs` | `OwnerBindingSpec.home_quota_gb`, `DEFAULT_HOME_QUOTA_GB`, `home_volume_name`, `is_home_volume`. |
| `deploy/k3s/crds.yaml` | Regenerated. |
| `crates/workspaces/src/k8s.rs` | `HOME_DIR`, `HOME_CLAIM`, `home_pv_name`, `HOME_LOCAL_DIRS`, the `home` pod volume + mount, prelude comment. |
| `crates/workspaces/src/engine/ops.rs` | `ensure_home_dirs`, `generation`/`parse_generation`, `sync_pool`, `materialize_home`. |
| `crates/workspaces/src/engine/pool.rs` | `.pushed-gen` read/write. |
| `bins/agent/src/binding.rs` | `ensure_home`: the home Volume, PV per namespace, PVC `home`, quota propagation. |
| `bins/agent/src/claim.rs` | `ensure_binding` fills `home_quota_gb`. |
| `bins/agent/src/controller.rs` | `ensure_child_volume(id, …)`, `Work.home`, `homes_to_push` + `spawn_home_push`, `StopPush`/`stop_push`, the workspace `Stopped` arm, the workspace controller's stop-request watch. |
| `bins/agent/tests/reconcile.rs` | Controller tests (binding, volume, stop, timer decision). |
| `crates/workspaces/tests/engine_ops.rs`, `engine_pool.rs` | btrfs test (skips off-Linux), `.pushed-gen` test. |
| `deploy/k3s/agent-rbac.yaml`, `agent-admission.yaml` | Table rows; `quotaGb` exception for OwnerBinding-owned Volumes. |
| `tests/ws_e2e.sh` | Home phase. |
| `CLAUDE.md`, `README.md` | Docs. |

---

### Task 1: `homeQuotaGb` on the binding and the home Volume's name

**Files:**
- Modify: `crates/workspaces/src/crd.rs:495-499` (`OwnerBindingSpec`), `:607-613` (next to `binding_name`), tests module at the end
- Modify: `bins/agent/src/claim.rs:365-368` (`ensure_binding`)
- Modify: `deploy/k3s/crds.yaml` (regenerated)

**Interfaces:**
- Produces:
  ```rust
  pub const DEFAULT_HOME_QUOTA_GB: u64 = 2;
  pub struct OwnerBindingSpec { pub owner: String, pub region: String, pub node_name: String, pub home_quota_gb: u64 }  // serde default = DEFAULT_HOME_QUOTA_GB
  pub fn home_volume_name(owner: &str) -> String;       // "home-{owner lowercased}", through dns_label
  pub fn is_home_volume(v: &Volume) -> bool;             // an ownerReference of kind "OwnerBinding"
  ```

- [ ] **Step 1: Write the failing tests** (append inside `mod tests` in `crd.rs`)

```rust
    /// The binding is created by whichever agent wins a placement claim, which has no opinion
    /// about quotas — so an object written without the field must read the default, and every
    /// binding that exists today (none carry it) must keep parsing.
    #[test]
    fn a_binding_without_a_home_quota_reads_the_default() {
        let b: OwnerBindingSpec =
            serde_json::from_value(serde_json::json!({"owner": "Alice", "region": "r1", "nodeName": "n"})).unwrap();
        assert_eq!(b.home_quota_gb, DEFAULT_HOME_QUOTA_GB);
        assert_eq!(DEFAULT_HOME_QUOTA_GB, 2);
    }

    #[test]
    fn the_home_volume_is_named_from_the_lowercased_owner() {
        assert_eq!(home_volume_name("Alice"), "home-alice");
        let mut v = Volume::new("home-alice", VolumeSpec {
            owner: "alice".into(), team: String::new(), node_name: "n".into(), region: "r1".into(),
            quota_gb: 2, source: None, restore_to: None,
        });
        assert!(!is_home_volume(&v), "a name is a convention, not the link");
        v.metadata.owner_references = Some(vec![k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
            api_version: "rustic-git.io/v1alpha1".into(), kind: "OwnerBinding".into(), name: "r1-alice".into(),
            uid: "u".into(), controller: Some(true), block_owner_deletion: Some(true),
        }]);
        assert!(is_home_volume(&v));
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p rustic-git-workspaces --lib crd::tests`
Expected: compile error — `home_quota_gb`, `DEFAULT_HOME_QUOTA_GB`, `home_volume_name`, `is_home_volume` not found.

- [ ] **Step 3: Implement**

In `crd.rs`, replace `OwnerBindingSpec`:

```rust
#[serde(rename_all = "camelCase")]
pub struct OwnerBindingSpec {
    pub owner: String,
    pub region: String,
    pub node_name: String,
    /// The cap on the owner's persistent home on this node, in GiB, copied into the home
    /// `Volume`'s `quotaGb` by the binding reconciler. Defaulted rather than required because the
    /// binding is created by whichever agent wins a placement claim (`claim::ensure_binding`),
    /// which has no opinion about quotas, and because every binding written before this field
    /// existed must keep parsing. An operator raises it with kubectl; the reconciler propagates.
    #[serde(default = "default_home_quota_gb")]
    pub home_quota_gb: u64,
}

/// Two gigabytes: dotfiles, shell history, editor state and a few tool configs. Caches are
/// nested subvolumes outside the quota (`k8s::HOME_LOCAL_DIRS`), so this is not where `node_modules`
/// goes and does not need to be sized for it.
pub const DEFAULT_HOME_QUOTA_GB: u64 = 2;
fn default_home_quota_gb() -> u64 {
    DEFAULT_HOME_QUOTA_GB
}
```

After `binding_name` add:

```rust
/// The home `Volume`'s name — and, through the ordinary `(owner, id)` keyspace, its registry name
/// `vol/{owner}/home-{owner}`. Nothing special-cases it: `GET /v1/volumes/home-{owner}/history`
/// answers like any volume's. Lowercased like every object name here, because a handle can carry
/// capitals and an object name cannot. Workspace ids are `ws-{hex}` and environments `env-{hex}`
/// (`api::rid`), so the `home-` prefix cannot collide with either.
pub fn home_volume_name(owner: &str) -> String {
    dns_label(&format!("home-{}", owner.to_lowercase()))
}

/// Whether a Volume is an owner's home: a child of an `OwnerBinding` rather than of a Workspace
/// or Environment. Read off the ownerReference, never the name — the name is a convention, the
/// reference is what garbage collection and the reconcilers actually act on.
pub fn is_home_volume(v: &Volume) -> bool {
    v.metadata.owner_references.as_ref().is_some_and(|refs| refs.iter().any(|r| r.kind == "OwnerBinding"))
}
```

In `bins/agent/src/claim.rs` `ensure_binding`, the struct literal becomes:

```rust
    let b = OwnerBinding::new(
        &name,
        OwnerBindingSpec {
            owner: owner.into(),
            region: region.into(),
            node_name: ctx.node.clone(),
            home_quota_gb: crd::DEFAULT_HOME_QUOTA_GB,
        },
    );
```

- [ ] **Step 4: Regenerate the CRD manifest and run the tests**

Run: `CRD_REGEN=1 cargo test -p rustic-git-workspaces --test crd_yaml && cargo test -p rustic-git-workspaces --lib crd::tests && cargo test -p rustic-git-agent`
Expected: all PASS; `git diff --stat deploy/k3s/crds.yaml` shows the `homeQuotaGb` property added under the OwnerBinding schema.

- [ ] **Step 5: Commit**

```bash
git add crates/workspaces/src/crd.rs bins/agent/src/claim.rs deploy/k3s/crds.yaml
git commit -m "Give a binding a home quota and name the owner's home volume"
```

---

### Task 2: The binding reconciler authors the home Volume, PV and claim

**Files:**
- Modify: `crates/workspaces/src/k8s.rs:255` (after `WORKSPACE_DIR`), `:609-615` (next to the nix PV names)
- Modify: `bins/agent/src/controller.rs:1096-1145` (`ensure_child_volume` takes the id), `:1272-1274` (the one caller in `resolve_volume`)
- Modify: `bins/agent/src/binding.rs:80-131` (`apply_binding`)
- Modify: `deploy/k3s/agent-rbac.yaml` (the table comment only)
- Test: `bins/agent/tests/reconcile.rs` (after `a_second_reconcile_of_a_ready_binding_writes_no_status`)

**Interfaces:**
- Consumes: `crd::home_volume_name`, `crd::DEFAULT_HOME_QUOTA_GB`, `OwnerBindingSpec.home_quota_gb` (Task 1); `controller::ensure`, `k8s::local_pv`, `k8s::claim`, `k8s::live_path`, `k8s::PodContext`.
- Produces:
  ```rust
  // k8s.rs
  pub const HOME_DIR: &str = "/home/kl";
  pub const HOME_CLAIM: &str = "home";
  pub fn home_pv_name(ns: &str) -> String;                       // "home-{ns}"
  // controller.rs — signature change, one extra leading parameter
  pub async fn ensure_child_volume<P>(id: &str, parent: &P, owner: &str, team: &str, region: &str,
      storage: &crd::WorkspaceStorage, node: &str, kind: &str, ctx: &Arc<Ctx>) -> Result<crd::Volume, ReconcileErr>;
  // binding.rs
  async fn ensure_home(b: &crd::OwnerBinding, owner_ref: &OwnerReference, namespaces: &[String], ctx: &Arc<Ctx>) -> Result<(), ReconcileErr>;
  ```

- [ ] **Step 1: Write the failing test** (in `reconcile.rs`, after `a_second_reconcile_of_a_ready_binding_writes_no_status`)

```rust
const HOME_VOL_GET: &str = "/apis/rustic-git.io/v1alpha1/volumes/home-alice";
const VOLUMES: &str = "/apis/rustic-git.io/v1alpha1/volumes";

fn home_vol_json(quota: u64) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "home-alice", "uid": "home-uid-1", "generation": 1,
                     "ownerReferences": [{"apiVersion": "rustic-git.io/v1alpha1", "kind": "OwnerBinding",
                                          "name": crd::binding_name("r1", "alice"), "uid": "ob-uid-1",
                                          "controller": true, "blockOwnerDeletion": true}]},
        "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": quota},
        "status": {"phase": "ready", "subvolumePresent": true},
    })
}

/// The home is authored next to the namespace, by the one reconciler that owns "this owner is on
/// this node": a child Volume with the binding as owner (so deleting the binding is the whole
/// delete), a local PV over its subvolume, and the fixed-name `home` claim a workspace pod mounts.
#[tokio::test]
async fn a_binding_creates_the_owners_home_volume_and_its_claim() {
    let tmp = tempfile::tempdir().unwrap();
    let ws_list = serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "WorkspaceList", "metadata": {},
        "items": [ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a"}))]
    });
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/workspaces", ws_list),
            Route { method: "PATCH", path: binding_status(), status: 200, body: binding_json() },
            rustic_git_workspaces::kube_test::not_found(HOME_VOL_GET),
            rustic_git_workspaces::kube_test::post(VOLUMES, home_vol_json(2)),
            pv_route("home-ws-alice"),
            pvc_route("home"),
        ]
        .into_iter()
        .chain(ns_routes("ws-alice"))
        .collect(),
    );
    let b: crd::OwnerBinding = serde_json::from_value(binding_json()).unwrap();

    rustic_git_agent::binding::apply_binding(&b, &ctx).await.unwrap();

    let vol = rec.sent("POST", VOLUMES);
    assert_eq!(vol.len(), 1, "{:?}", rec.calls());
    assert_eq!(vol[0]["metadata"]["name"], "home-alice");
    assert_eq!(vol[0]["spec"]["owner"], "alice");
    assert_eq!(vol[0]["spec"]["team"], "");
    assert_eq!(vol[0]["spec"]["nodeName"], "node-a", "the binding's node, never chosen here");
    assert_eq!(vol[0]["spec"]["quotaGb"], 2, "the binding's default home quota");
    assert!(vol[0]["spec"].get("source").is_none(), "an empty home; the first materialization decides whether to pull");
    assert_eq!(vol[0]["metadata"]["ownerReferences"][0]["kind"], "OwnerBinding");
    assert_eq!(vol[0]["metadata"]["labels"]["rustic-git.io/kind"], "home");

    let pv = rec.sent("PATCH", "/api/v1/persistentvolumes/home-ws-alice");
    assert_eq!(pv.len(), 1, "{:?}", rec.calls());
    assert_eq!(pv[0]["spec"]["local"]["path"], format!("{}/vol/home-alice/live", tmp.path().display()));
    assert_eq!(pv[0]["spec"]["accessModes"][0], "ReadWriteOnce");
    assert_eq!(pv[0]["metadata"]["ownerReferences"][0]["kind"], "OwnerBinding");
    let pvc = rec.sent("PATCH", "/api/v1/namespaces/ws-alice/persistentvolumeclaims/home");
    assert_eq!(pvc.len(), 1, "{:?}", rec.calls());
    assert_eq!(pvc[0]["spec"]["volumeName"], "home-ws-alice", "bound to THIS namespace's PV, never whichever fits");
    assert!(
        rec.sent("PATCH", &binding_status()).iter().any(|s| s["status"]["conditions"].as_array().unwrap()
            .iter().any(|c| c["type"] == "NamespaceReady" && c["status"] == "True")),
        "the namespace is still reported ready: the home is not a gate"
    );
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p rustic-git-agent --test reconcile a_binding_creates_the_owners_home_volume_and_its_claim`
Expected: FAIL — `rec.sent("POST", VOLUMES)` is empty (no home is authored yet).

- [ ] **Step 3: Add the names to `k8s.rs`**

After `pub const WORKSPACE_DIR`:

```rust
/// Where the owner's persistent home is mounted; `WORKSPACE_DIR` is inside it. Everything under
/// here except `workspace` is the same in every workspace the person opens on this node.
pub const HOME_DIR: &str = "/home/kl";
/// The claim every workspace pod in a namespace mounts at `HOME_DIR`. A fixed name: there is one
/// home per (owner, namespace), so an id would only repeat what the namespace already says.
pub const HOME_CLAIM: &str = "home";
```

After `nix_claim_name`:

```rust
/// The local PV behind `HOME_CLAIM` in `ns`. Per NAMESPACE rather than per owner: a local PV
/// binds to exactly one claim, and an owner with workspaces in two teams has two namespaces that
/// each need their own claim on the one host path. Cluster-scoped, so the namespace is in the name.
pub fn home_pv_name(ns: &str) -> String {
    format!("home-{ns}")
}
```

- [ ] **Step 4: Let `ensure_child_volume` take the child's id**

In `controller.rs` change the signature and the first line:

```rust
/// Create a `Volume` child if it is missing, and hand back what the API server holds.
///
/// A parent's child takes the PARENT's name: the id is already the registry key, the PV name, the
/// PVC name and the URL segment, and an ownerReference — not a name — is what makes it a child.
/// That ownerReference is also the whole delete story: `DELETE workspace` reclaims the disk with no
/// ordering logic anywhere in the API. The one child that is NOT named after its parent is an
/// owner's home (`crd::home_volume_name`), whose parent is the binding — hence `id` is a parameter.
#[allow(clippy::too_many_arguments)]
pub async fn ensure_child_volume<P>(
    id: &str,
    parent: &P,
    owner: &str,
    team: &str,
    region: &str,
    storage: &crd::WorkspaceStorage,
    node: &str,
    kind: &str,
    ctx: &Arc<Ctx>,
) -> Result<crd::Volume, ReconcileErr>
where
    P: Resource<DynamicType = ()> + ResourceExt,
{
    let api: Api<crd::Volume> = Api::all(ctx.client.clone());
    if let Some(v) = api.get_opt(id).await? {
        return Ok(v);
    }
    let mut vol = crd::Volume::new(
        id,
        // … the VolumeSpec literal is unchanged …
```

(delete the old `let id = parent.name_any();` line; the later `api.get(&id)` becomes `api.get(id)`.) The caller in `resolve_volume` becomes:

```rust
        (Some(s), _) => {
            ensure_child_volume(&parent.name_any(), parent, owner, team, region, s, node_name, &api_kind.to_lowercase(), ctx).await?
        }
```

- [ ] **Step 5: Author the home in `binding.rs`**

Add imports: `use k8s_openapi::api::core::v1::{LimitRange, Namespace, PersistentVolume, PersistentVolumeClaim};`, `use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;`, and `use crate::controller::ensure_child_volume;`. Add the function:

```rust
/// The owner's home on this node: one child `Volume` (`home-{owner}`) with the binding as owner,
/// a local PV over its subvolume in every namespace this owner has here, and the fixed-name
/// `home` claim each of those PVs binds to.
///
/// Authored HERE and not by the workspace reconciler for the same reason the namespace is: the
/// home is shared by every workspace of this owner on this node, so a per-workspace owner would
/// garbage-collect it with the first workspace deleted. It is NOT a gate on `NamespaceReady` — a
/// pod that starts before its claim exists sits `Pending` on the PVC and picks it up when this
/// reconciler lands, which is one `TICK` at most.
///
/// The PV/PVC capacity is the DEFAULT quota, always, not the binding's current one: a PVC's
/// storage request cannot shrink and grows only with `allowVolumeExpansion` on the class, so
/// re-applying a changed number is a permanent reconcile error. The number is nominal for a local
/// PV — the btrfs qgroup on the Volume is the real cap, and that one does follow the binding.
async fn ensure_home(
    b: &crd::OwnerBinding,
    owner_ref: &OwnerReference,
    namespaces: &[String],
    ctx: &Arc<Ctx>,
) -> Result<(), ReconcileErr> {
    let owner = &b.spec.owner;
    let id = crd::home_volume_name(owner);
    let storage = crd::WorkspaceStorage { quota_gb: b.spec.home_quota_gb, source: None };
    ensure_child_volume(&id, b, owner, "", &b.spec.region, &storage, &b.spec.node_name, "home", ctx).await?;
    let pod_ctx = k8s::PodContext {
        pool: &ctx.pool,
        node_name: &b.spec.node_name,
        owner_ref: owner_ref.clone(),
        runtime_class: None,
        default_image: "",
    };
    let live = k8s::live_path(&ctx.pool, &id);
    for ns in namespaces {
        let pv = k8s::home_pv_name(ns);
        ensure(
            &Api::<PersistentVolume>::all(ctx.client.clone()),
            &k8s::local_pv(&pv, &live, "ReadWriteOnce", crd::DEFAULT_HOME_QUOTA_GB, owner, &pod_ctx),
            ctx,
        )
        .await?;
        ensure(
            &Api::<PersistentVolumeClaim>::namespaced(ctx.client.clone(), ns),
            &k8s::claim(ns, k8s::HOME_CLAIM, &pv, "ReadWriteOnce", crd::DEFAULT_HOME_QUOTA_GB, owner, owner_ref),
            ctx,
        )
        .await?;
    }
    Ok(())
}
```

In `apply_binding`, collect the namespaces and call it before the status write:

```rust
    let owner = &b.spec.owner;
    let mut namespaces = Vec::new();
    for team in teams_in_use(ctx, owner).await? {
        let ns = ws_namespace(owner, &team);
        // … the existing per-namespace ensures, unchanged …
        namespaces.push(ns);
    }
    ensure_home(b, &owner_ref, &namespaces, ctx).await?;
    write_binding_status(b, ctx, gen).await?;
    Ok(Action::await_change())
```

- [ ] **Step 6: Update the RBAC table comment** in `deploy/k3s/agent-rbac.yaml` (rows only; the rules already grant these):

```
#   volumes (rustic-git.io)                get,list,watch               Controller + heartbeat list
#                                          create                       ensure_child_volume (a
#                                                                       parent's child; binding::
#                                                                       ensure_home for home-{owner})
…
#   limitranges, networkpolicies, services,
#   persistentvolumeclaims                 create,patch                 ensure (incl. the `home`
#                                                                       claim, binding::ensure_home)
#   persistentvolumes                      create,patch                 ensure (cluster-scoped; incl.
#                                                                       home-{ns}, binding::ensure_home)
```

- [ ] **Step 7: Run the tests**

Run: `cargo test -p rustic-git-agent --test reconcile binding && cargo test -p rustic-git-agent && cargo clippy -p rustic-git-agent --all-targets -- -D warnings`
Expected: all PASS, including the two pre-existing binding tests — `a_second_reconcile_of_a_ready_binding_writes_no_status` needs its route list extended with `not_found(HOME_VOL_GET)`, `post(VOLUMES, home_vol_json(2))`, `pv_route("home-ws-alice")`, `pvc_route("home")` to stay green (the mock 404s unknown paths, and `ensure` on a 404 is an error). Do the same for `a_binding_ensures_one_namespace_per_team_in_use_and_reports_ready`, adding `pv_route(&format!("home-{}", crd::ws_namespace("alice", "acme")))` and a PVC route for that namespace (`pvc_route` is hard-wired to `ws-alice`; add a `Route { method: "PATCH", path: format!("/api/v1/namespaces/{acme}/persistentvolumeclaims/home"), status: 200, body: … }` inline).

- [ ] **Step 8: Commit**

```bash
git add crates/workspaces/src/k8s.rs bins/agent/src/controller.rs bins/agent/src/binding.rs bins/agent/tests/reconcile.rs deploy/k3s/agent-rbac.yaml
git commit -m "Author an owner's home volume, PV and claim from the binding reconciler"
```

---

### Task 3: Nested cache subvolumes, generation, and the Volume controller's home arm

**Files:**
- Modify: `crates/workspaces/src/k8s.rs` (after `HOME_CLAIM`)
- Modify: `crates/workspaces/src/engine/ops.rs:233-240` (after `set_quota`)
- Modify: `bins/agent/src/controller.rs:797-871` (`Work`, `volume_work`), `:751-760` (the `Work` literal in `apply_volume`)
- Test: `crates/workspaces/tests/engine_ops.rs` (btrfs, skips off-Linux), `crates/workspaces/src/engine/ops.rs` unit test for the parser

**Interfaces:**
- Consumes: `k8s::SSH_UID` (i64, = 1000), `crd::is_home_volume` (Task 1).
- Produces:
  ```rust
  // k8s.rs
  pub const HOME_LOCAL_DIRS: [&str; 4] = [".cache", ".npm", ".cargo/registry", ".local/share/pnpm"];
  // ops.rs
  impl Engine {
      pub fn ensure_home_dirs(&self, id: &str, uid: u32) -> Result<(), EngErr>;
      pub fn generation(&self, id: &str) -> Result<u64, EngErr>;
      pub fn sync_pool(&self) -> Result<(), EngErr>;
  }
  pub fn parse_generation(subvolume_show: &str) -> Option<u64>;
  // controller.rs
  pub struct Work { …, pub home: bool }
  ```

- [ ] **Step 1: Write the failing tests**

Unit test, inside a `#[cfg(test)] mod tests` that already exists at the bottom of `ops.rs` (line ~1097, `fn engine(root)` lives there):

```rust
    #[test]
    fn the_generation_is_read_off_subvolume_show() {
        let out = "vol/home-alice/live\n\tName: \t\t\tlive\n\tUUID: \t\t\t1234\n\tCreation time: \t\t2026-08-29 10:00:00 +0000\n\tSubvolume ID: \t\t257\n\tGeneration: \t\t4711\n\tGen at creation: \t7\n\tFlags: \t\t\t-\n";
        assert_eq!(super::parse_generation(out), Some(4711));
        assert_eq!(super::parse_generation("nothing here"), None);
    }
```

btrfs test in `crates/workspaces/tests/engine_ops.rs` (append; uses the file's `LoopbackPool`, `registry_server`, `engine`, `history`, `run` fixtures):

```rust
/// The whole point of nesting: `btrfs send` never descends into a nested subvolume and the parent's
/// qgroup does not count it, so a cache never uploads and never eats the home's quota — and a
/// restore, which receives a stream with no trace of them, has to make them again.
#[tokio::test]
async fn a_homes_cache_subvolumes_stay_out_of_the_push_and_come_back_after_a_restore() {
    use std::os::unix::fs::MetadataExt;
    if !have_btrfs() {
        eprintln!("skipping: btrfs unavailable or not root");
        return;
    }
    let lp = LoopbackPool::new();
    let base = registry_server().await;
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let e = engine(lp.pool(), store.clone(), Arc::new(MemStore::new()), &base);
    e.create_subvol("home-alice").unwrap();
    e.ensure_home_dirs("home-alice", 1000).unwrap();
    let live = e.pool.live("home-alice");
    for rel in rustic_git_workspaces::k8s::HOME_LOCAL_DIRS {
        assert!(live.join(rel).is_dir(), "{rel}");
        assert_eq!(std::fs::metadata(live.join(rel)).unwrap().uid(), 1000, "{rel} must be the owner's, not root's");
    }
    assert_eq!(std::fs::metadata(live.join(".cargo")).unwrap().uid(), 1000, "the parent dir too, or `mkdir ~/.cargo/x` fails as kl");
    // A nested subvolume has its own inode 256, a plain directory does not.
    assert_eq!(std::fs::metadata(live.join(".cache")).unwrap().ino(), 256);
    std::fs::write(live.join(".zshrc"), b"alias ll='ls -l'").unwrap();
    std::fs::write(live.join(".cache").join("big"), vec![1u8; 1 << 20]).unwrap();

    e.sync_pool().unwrap();
    let g1 = e.generation("home-alice").unwrap();
    e.push_env("alice", "home-alice", &serde_json::Value::Null, Some("home: periodic")).await.unwrap();
    // Idempotent: everything is present, nothing is recreated, nothing is lost.
    e.ensure_home_dirs("home-alice", 1000).unwrap();
    assert!(live.join(".cache").join("big").exists());
    std::fs::write(live.join("touched"), b"x").unwrap();
    e.sync_pool().unwrap();
    assert!(e.generation("home-alice").unwrap() > g1, "a write moves the generation");

    let tip = history(&base, "alice", "home-alice").await[0].id.clone();
    e.restore("alice", "home-alice", &tip, "home-alice-2", None).await.unwrap();
    let live2 = e.pool.live("home-alice-2");
    assert_eq!(std::fs::read(live2.join(".zshrc")).unwrap(), b"alias ll='ls -l'");
    assert!(!live2.join(".cache").join("big").exists(), "nested subvolumes are never in the send stream");
    e.ensure_home_dirs("home-alice-2", 1000).unwrap();
    for rel in rustic_git_workspaces::k8s::HOME_LOCAL_DIRS {
        assert!(live2.join(rel).is_dir(), "{rel} must be recreated after a restore");
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p rustic-git-workspaces --lib the_generation_is_read_off_subvolume_show; cargo test -p rustic-git-workspaces --test engine_ops a_homes_cache`
Expected: compile errors — `parse_generation`, `ensure_home_dirs`, `sync_pool`, `generation`, `HOME_LOCAL_DIRS` not found. (On this Mac the btrfs test prints `skipping` once it compiles; it runs for real on the Linux VM.)

- [ ] **Step 3: The constant** (in `k8s.rs`, after `HOME_CLAIM`)

```rust
/// What inside a home is a nested subvolume rather than a directory: package caches. btrfs `send`
/// skips a nested subvolume and the home's qgroup does not count it, so these never upload and
/// never eat the quota. ONE list, read by the create path and the restore path in the engine — two
/// lists would drift and a cache would come back as a plain directory that the next push carries.
/// A person who wants something else excluded runs `btrfs subvolume create` themselves; that is the
/// documented escape hatch, not a UI.
pub const HOME_LOCAL_DIRS: [&str; 4] = [".cache", ".npm", ".cargo/registry", ".local/share/pnpm"];
```

- [ ] **Step 4: The engine helpers** (in `ops.rs`, after `set_quota`)

```rust
    /// The nested subvolumes that keep a home's caches out of every push and out of its quota
    /// (`k8s::HOME_LOCAL_DIRS`). Run after EVERY path that leaves a new `live` behind — create,
    /// pull, restore — because a received stream carries no trace of them: without this `.cache`
    /// comes back as nothing at all and the next `npm install` writes it INTO the home.
    ///
    /// Keep-biased: an entry that already exists — as a subvolume, or as a plain directory the
    /// person made themselves — is left exactly as it is. Every directory made here is chowned to
    /// the owner, parents included: root-made `~/.cargo` is a `mkdir ~/.cargo/x: Permission denied`
    /// for the person the home belongs to.
    pub fn ensure_home_dirs(&self, id: &str, uid: u32) -> Result<(), EngErr> {
        let live = self.pool.live(id);
        for rel in crate::k8s::HOME_LOCAL_DIRS {
            let p = live.join(rel);
            if p.exists() {
                continue;
            }
            let mut made = Vec::new();
            let mut d = p.parent().map(std::path::Path::to_path_buf).unwrap_or_else(|| live.clone());
            while d != live && !d.exists() {
                made.push(d.clone());
                d = d.parent().map(std::path::Path::to_path_buf).unwrap_or_else(|| live.clone());
            }
            for d in made.iter().rev() {
                std::fs::create_dir(d).map_err(EngErr::io)?;
                std::os::unix::fs::chown(d, Some(uid), Some(uid)).map_err(EngErr::io)?;
            }
            run(&["btrfs", "subvolume", "create", p.to_str().unwrap()])?;
            std::os::unix::fs::chown(&p, Some(uid), Some(uid)).map_err(EngErr::io)?;
        }
        Ok(())
    }

    /// The btrfs generation of `id`'s live subvolume: a counter the filesystem bumps on every
    /// committed transaction that touched it, so "has anything changed since the last push" is one
    /// `subvolume show` rather than a walk of the tree.
    pub fn generation(&self, id: &str) -> Result<u64, EngErr> {
        let live = self.pool.live(id);
        let out = std::process::Command::new("btrfs")
            .args(["subvolume", "show", live.to_str().unwrap()])
            .output()
            .map_err(EngErr::io)?;
        if !out.status.success() {
            return Err(EngErr::other(format!(
                "btrfs subvolume show {}: {}",
                live.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        parse_generation(&String::from_utf8_lossy(&out.stdout))
            .ok_or_else(|| EngErr::other(format!("btrfs subvolume show {}: no Generation line", live.display())))
    }

    /// Commit the pool's open transaction. `generation` reads the COMMITTED number, and btrfs
    /// commits on its own only every ~30s — so a beat that reads without this can miss a write
    /// made just before it. One call per beat, not per home.
    pub fn sync_pool(&self) -> Result<(), EngErr> {
        run(&["btrfs", "filesystem", "sync", self.pool.root.to_str().unwrap()])
    }
```

And a free function near `fn run`:

```rust
/// The `Generation:` line of `btrfs subvolume show`. Split from the command so the parse has a test
/// that runs where btrfs does not.
pub fn parse_generation(subvolume_show: &str) -> Option<u64> {
    subvolume_show
        .lines()
        .find_map(|l| l.trim().strip_prefix("Generation:"))
        .and_then(|g| g.trim().parse().ok())
}
```

- [ ] **Step 5: Wire the home arm into `volume_work`**

`Work` gains `pub home: bool`; `apply_volume` fills it with `home: crd::is_home_volume(v)` (`let home = crd::is_home_volume(v);` next to `let quota_gb = …`, and `Work { id, owner, source, materialize, restore, quota_gb, home }`). In `volume_work`, destructure `home` too, and after the restore block (before `set_quota`):

```rust
        // After every path that can leave a new `live` behind, same rule as the quota below: a
        // home's caches are nested subvolumes, and a received stream does not carry them.
        if home {
            engine.ensure_home_dirs(id, rustic_git_workspaces::k8s::SSH_UID as u32).map_err(|e| e.to_string())?;
        }
```

(The materialize arm for a home changes in Task 5; for now a home with `source: None` goes through `create_subvol` like any other.)

- [ ] **Step 6: Run the tests**

Run: `cargo test -p rustic-git-workspaces --lib the_generation_is_read_off_subvolume_show && cargo test -p rustic-git-workspaces --test engine_ops a_homes_cache && cargo test -p rustic-git-agent && cargo clippy -p rustic-git-workspaces -p rustic-git-agent --all-targets -- -D warnings`
Expected: the parser test PASS; the btrfs test prints `skipping: btrfs unavailable or not root` and PASS here (real run on the VM: PASS); the agent suite PASS; clippy clean.

- [ ] **Step 7: Commit**

```bash
git add crates/workspaces/src/k8s.rs crates/workspaces/src/engine/ops.rs crates/workspaces/tests/engine_ops.rs bins/agent/src/controller.rs
git commit -m "Keep a home's caches in nested subvolumes and read its btrfs generation"
```

---

### Task 4: The workspace pod mounts the home under the workspace

**Files:**
- Modify: `crates/workspaces/src/k8s.rs:336-354` (`prelude` doc comment), `:686-700` (next to `claim_volume`), `:815-905` (`workspace_pod`)
- Test: `crates/workspaces/src/k8s.rs` tests module

**Interfaces:**
- Consumes: `HOME_DIR`, `HOME_CLAIM` (Task 2).
- Produces: `workspace_pod` has a pod volume `home` (PVC `HOME_CLAIM`) mounted read-write at `HOME_DIR`, listed before `live` at `WORKSPACE_DIR`.

- [ ] **Step 1: Write the failing test** (in `k8s.rs` `mod tests`, after `a_workspace_pod_mounts_its_volume_at_workspace_and_only_there`)

```rust
    /// The home is a PV mounted at `/home/kl` and the workspace subvolume a PV mounted INSIDE it;
    /// the kubelet orders mounts by path depth, so the paths carry the order. The ssh Secret
    /// mounts under `/home/kl/.ssh` land inside the home too — a Secret inside a PV is fine.
    #[test]
    fn a_workspace_pod_mounts_the_home_and_the_workspace_inside_it() {
        let p = workspace_pod(&ws_spec(), "ws-1", &ctx(), None);
        let s = p.spec.unwrap();
        let home = s.volumes.as_ref().unwrap().iter().find(|v| v.name == "home").expect("home volume");
        assert_eq!(home.persistent_volume_claim.as_ref().unwrap().claim_name, HOME_CLAIM);
        let mounts = s.containers[0].volume_mounts.as_ref().unwrap();
        let home_mount = mounts.iter().find(|m| m.name == "home").expect("home mount");
        assert_eq!(home_mount.mount_path, HOME_DIR);
        assert!(home_mount.read_only.is_none(), "dotfiles are written by the person");
        assert!(home_mount.sub_path.is_none());
        let live = mounts.iter().find(|m| m.name == "live").unwrap();
        assert!(live.mount_path.starts_with(&format!("{HOME_DIR}/")), "the workspace is INSIDE the home: {}", live.mount_path);
        assert!(SSH_HOME.starts_with(HOME_DIR));
        // A custom image gets the home too: it is the person's, not the image's.
        let mut custom = ws_spec();
        custom.image = "ghcr.io/someone/theirs:1".into();
        let s = workspace_pod(&custom, "ws-1", &ctx(), None).spec.unwrap();
        assert!(s.volumes.as_ref().unwrap().iter().any(|v| v.name == "home"));
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p rustic-git-workspaces --lib a_workspace_pod_mounts_the_home_and_the_workspace_inside_it`
Expected: FAIL — `expected home volume`.

- [ ] **Step 3: Implement**

Next to `claim_volume`:

```rust
/// The owner's home, one claim per namespace (`HOME_CLAIM`), authored by the binding reconciler
/// before any pod of theirs exists here. Read-write: it is the person's dotfiles.
fn home_volume() -> Volume {
    Volume {
        name: "home".to_string(),
        persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
            claim_name: HOME_CLAIM.to_string(),
            read_only: Some(false),
        }),
        ..Default::default()
    }
}
```

In `workspace_pod`, the container's `volume_mounts` vec starts with the home mount (before `live`):

```rust
            volume_mounts: Some(vec![
                // Listed before the workspace mount for the reader; the kubelet orders by path
                // depth and `WORKSPACE_DIR` is under `HOME_DIR`, so the order is implied either way.
                VolumeMount { name: "home".to_string(), mount_path: HOME_DIR.to_string(), ..Default::default() },
                VolumeMount {
                    name: "live".to_string(),
                    mount_path: WORKSPACE_DIR.to_string(),
                    ..Default::default()
                },
                // … the rest unchanged …
```

and the pod's `volumes` list becomes `vec![home_volume(), claim_volume(id), nix_volume(id), user_key_volume(init.is_some())]`.

Replace the last two sentences of `prelude`'s doc comment (`ponytail: chown -R … a persistent home is the upgrade.`) with:

```rust
/// ponytail: `chown -R` walks the whole volume on every start; fine for source trees. `$H` is the
/// persistent home PV and the rc files are seeded only if absent, so a person's own edits survive
/// a restart and a new workspace alike; `~/workspace` is a mount point inside it that the kubelet
/// makes, which is why nothing here mkdirs it.
```

The prelude's script body is unchanged (`mkdir -p $H/.config/fish` is already a no-op when present, and the `[ -e … ] ||` guards already seed once).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p rustic-git-workspaces --lib k8s && cargo test -p rustic-git-agent --test reconcile`
Expected: PASS. If `every_child_object_cascades_on_delete` or `a_git_seeded_pod_carries_an_init_container_with_the_key_and_no_token` index into the volume/mount lists by position, adjust them to look up by name — the assertions themselves are unchanged.

- [ ] **Step 5: Commit**

```bash
git add crates/workspaces/src/k8s.rs
git commit -m "Mount the owner's home at /home/kl with the workspace inside it"
```

---

### Task 5: First materialization pulls the registry's `main` when this node has no home

**Files:**
- Modify: `crates/workspaces/src/engine/ops.rs` (after `pull_env`)
- Modify: `bins/agent/src/controller.rs:808-830` (`volume_work` materialize arm)
- Test: `bins/agent/tests/reconcile.rs` (after `a_reconcile_that_cannot_read_the_pool_deletes_nothing`)

**Interfaces:**
- Consumes: `Work.home` (Task 3), `ensure_home_dirs` (Task 3), `REGION_UNREACHABLE`, `pull_core`, `create_subvol`, `RegistryClient::get_history`.
- Produces: `impl Engine { pub async fn materialize_home(&self, owner: &str, id: &str) -> Result<(), EngErr>; }`

- [ ] **Step 1: Write the failing test** (in `reconcile.rs`)

```rust
const HOME_STATUS: &str = "/apis/rustic-git.io/v1alpha1/volumes/home-alice/status";

fn home_volume() -> crd::Volume {
    serde_json::from_value(home_vol_json(2)).map(|mut v: crd::Volume| { v.status = None; v }).unwrap()
}

/// Keep-biased, on the one failure that matters for a home: a node that has never seen this owner
/// asks the registry whether a copy exists, and if it cannot ask, it makes NOTHING. An empty home
/// created "for now" and overwritten by the copy later is the silent loss a person notices a week
/// on. `RegionUnreachable` is permanent, so the object settles instead of retrying every minute.
#[tokio::test]
async fn a_home_that_cannot_reach_the_registry_settles_and_creates_no_subvolume() {
    let tmp = tempfile::tempdir().unwrap();
    // Port 1: the `ctx` helper's registry, which nothing listens on.
    let (ctx, rec) = ctx(tmp.path(), vec![patch_ok(HOME_STATUS)]);
    let v = home_volume();

    let action = rustic_git_agent::controller::apply_volume(&v, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)));
    wait_idle(&ctx).await;
    let action = rustic_git_agent::controller::apply_volume(&v, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "permanent: no retry loop");

    let last = rec.sent("PATCH", HOME_STATUS).last().cloned().expect("a status write");
    assert_eq!(last["status"]["phase"], "error");
    assert!(
        last["status"]["conditions"].as_array().unwrap().iter().any(|c| c["reason"] == "RegionUnreachable"),
        "{last}"
    );
    assert!(!tmp.path().join("vol/home-alice/live").exists(), "nothing was created empty");
    assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE")), "{:?}", rec.calls());
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p rustic-git-agent --test reconcile a_home_that_cannot_reach_the_registry`
Expected: FAIL — on this Mac the operation fails with a `btrfs` not-found error, reason `OperationFailed`, action `requeue(60s)`, not `RegionUnreachable`/`await_change` (the home arm goes to `create_subvol` and never asks the registry).

- [ ] **Step 3: Implement**

In `ops.rs`, after `pull_env`:

```rust
    /// A home's first materialization on this node: the registry's `main` ref when there is one,
    /// an empty subvolume when there is not. A `live` that already exists is never touched — local
    /// is truth on its node and the registry is the copy, so a node that has the home never pulls
    /// over it, whatever the registry says.
    ///
    /// The registry being unreachable is `REGION_UNREACHABLE`, permanent, and creates NOTHING:
    /// "no history" and "could not ask" must not look alike, because an empty home made on the
    /// second and overwritten later is the one loss this whole feature exists to prevent.
    pub async fn materialize_home(&self, owner: &str, id: &str) -> Result<(), EngErr> {
        if self.pool.live(id).exists() {
            return Ok(());
        }
        let history = self
            .registry
            .get_history(owner, id)
            .await
            .map_err(|e| EngErr::other(format!("{REGION_UNREACHABLE}: registry history for {owner}/{id}: {e}")))?;
        match history.first() {
            Some(tip) => {
                self.pull_core(id, tip.lineage.clone(), &self.store).await?;
                Ok(())
            }
            None => self.create_subvol(id),
        }
    }
```

In `controller.rs` `volume_work`, the materialize arm becomes:

```rust
        if materialize {
            crate::record_owner(&engine.pool.root.to_string_lossy(), id, owner);
            if home {
                // The registry's copy when this node has none, else nothing: a home is never
                // created from a `source`, and the API never writes one for it.
                engine.materialize_home(owner, id).await.map_err(|e| e.to_string())?;
            } else {
                match &source {
                    // … the existing four arms, unchanged …
                }
            }
        }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p rustic-git-agent --test reconcile && cargo clippy -p rustic-git-workspaces -p rustic-git-agent --all-targets -- -D warnings`
Expected: PASS; `permanent_reason` already maps the `region unreachable` marker to `RegionUnreachable`, so no change there.

- [ ] **Step 5: Commit**

```bash
git add crates/workspaces/src/engine/ops.rs bins/agent/src/controller.rs bins/agent/tests/reconcile.rs
git commit -m "Pull a home from the registry the first time a node materializes it"
```

---

### Task 6: The periodic home push

**Files:**
- Modify: `crates/workspaces/src/engine/pool.rs:10-65` (`impl Pool`)
- Modify: `bins/agent/src/controller.rs:259-262` (`run`: spawn the beat), `:506-530` (next to `spawn_snapshot_gc`), `:1148-1150` (`volume_is_ready` becomes `pub(crate)`)
- Test: `crates/workspaces/tests/engine_pool.rs`, `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Consumes: `Engine::generation`, `Engine::sync_pool` (Task 3), `Engine::push_env`, `crd::is_home_volume` (Task 1), `Ctx::volumes`, `running_contains`.
- Produces:
  ```rust
  // pool.rs
  impl Pool {
      pub fn pushed_gen_path(&self, name: &str) -> PathBuf;                 // {pool}/vol/{name}/.pushed-gen
      pub fn pushed_gen(&self, name: &str) -> Option<u64>;
      pub fn record_pushed_gen(&self, name: &str, generation: u64) -> Result<(), String>;
  }
  // controller.rs
  pub const HOME_PUSH_MESSAGE: &str = "home: periodic";
  pub fn home_push_interval() -> Duration;                                   // WS_HOME_PUSH_SECS, default 300
  pub fn homes_to_push(volumes: &[Arc<crd::Volume>], generation: impl Fn(&str) -> Option<u64>, pushed: impl Fn(&str) -> Option<u64>) -> Vec<Arc<crd::Volume>>;
  fn spawn_home_push(ctx: Arc<Ctx>);
  ```
- Note on "a fake engine": `Engine` is a concrete struct, not a trait, and making it one for a beat is an abstraction with one implementation. The beat is therefore split so the DECISION (`homes_to_push`) is pure and tested with closures, the FILE half (`pushed_gen`/`record_pushed_gen`) is tested on a tempdir, and the push itself (`push_env`) is covered by the e2e in Task 10.

- [ ] **Step 1: Write the failing tests**

`crates/workspaces/tests/engine_pool.rs` (append):

```rust
/// The generation file is the timer's whole memory: absent means "never pushed" (push), a number
/// means "push only if the disk moved past it". Written tmp+rename like the lineage, so a crash
/// mid-write reads as absent — one extra push, never a skipped one.
#[test]
fn the_pushed_generation_round_trips_and_is_absent_until_recorded() {
    let tmp = tempfile::tempdir().unwrap();
    let pool = Pool::new(tmp.path());
    std::fs::create_dir_all(pool.voldir("home-alice")).unwrap();
    assert_eq!(pool.pushed_gen("home-alice"), None);
    pool.record_pushed_gen("home-alice", 4711).unwrap();
    assert_eq!(pool.pushed_gen("home-alice"), Some(4711));
    assert_eq!(pool.pushed_gen_path("home-alice"), tmp.path().join("vol/home-alice/.pushed-gen"));
    assert!(!tmp.path().join("vol/home-alice/.pushed-gen.tmp").exists());
    std::fs::write(pool.pushed_gen_path("home-alice"), b"garbage").unwrap();
    assert_eq!(pool.pushed_gen("home-alice"), None, "unreadable is absent, which pushes");
    assert!(pool.record_pushed_gen("nowhere", 1).is_err(), "a missing voldir is an error, not a panic");
}
```

`bins/agent/tests/reconcile.rs` (append, next to the other pure-function tests such as `a_volume_event_wakes_the_requests_that_name_it`):

```rust
/// The timer's decision, with the two numbers it reads faked: only homes, only ready ones, only
/// those whose disk moved since the recorded push — and a home whose generation cannot be read is
/// skipped rather than pushed blind.
#[test]
fn only_changed_ready_homes_are_pushed_by_the_timer() {
    let home = |name: &str, ready: bool| -> Arc<crd::Volume> {
        let mut v = home_vol_json(2);
        v["metadata"]["name"] = serde_json::json!(name);
        if !ready {
            v["status"] = serde_json::json!({"phase": "working", "subvolumePresent": false});
        }
        Arc::new(serde_json::from_value(v).unwrap())
    };
    let ws: Arc<crd::Volume> = Arc::new(serde_json::from_value(vol_on("node-a")).unwrap());
    let volumes = vec![home("home-moved", true), home("home-same", true), home("home-new", true), home("home-unreadable", true), home("home-working", true), ws];
    let generation = |id: &str| match id {
        "home-moved" => Some(11),
        "home-same" => Some(10),
        "home-new" => Some(3),
        "home-working" => Some(9),
        "ws-1" => Some(99),
        _ => None,
    };
    let pushed = |id: &str| match id {
        "home-moved" | "home-same" => Some(10),
        "home-working" => Some(9),
        _ => None,
    };
    let mut due: Vec<String> = rustic_git_agent::controller::homes_to_push(&volumes, generation, pushed)
        .iter().map(|v| v.metadata.name.clone().unwrap()).collect();
    due.sort();
    assert_eq!(due, vec!["home-moved", "home-new"]);
    assert_eq!(rustic_git_agent::controller::HOME_PUSH_MESSAGE, "home: periodic");
}
```

(`vol_on` is the existing helper at line ~1093 that builds a plain workspace Volume on a node; `home_vol_json` is from Task 2.)

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p rustic-git-workspaces --test engine_pool the_pushed_generation; cargo test -p rustic-git-agent --test reconcile only_changed_ready_homes`
Expected: compile errors — `pushed_gen`, `record_pushed_gen`, `homes_to_push`, `HOME_PUSH_MESSAGE` not found.

- [ ] **Step 3: The pool half** (in `pool.rs`, inside `impl Pool` after `set_lineage`)

```rust
    /// `{pool}/vol/{name}/.pushed-gen` — the btrfs generation recorded after the timer's last
    /// push of a home. Inside the voldir, next to `live`, and outside the subvolume: it must not
    /// be in the stream, and it must die with the volume (`cleanup_local` removes the voldir).
    pub fn pushed_gen_path(&self, name: &str) -> PathBuf {
        self.voldir(name).join(".pushed-gen")
    }
    /// `None` is "never pushed, or unreadable" — both push, because an extra push is cheap and a
    /// skipped one is a home whose last hour is on one disk.
    pub fn pushed_gen(&self, name: &str) -> Option<u64> {
        std::fs::read_to_string(self.pushed_gen_path(name)).ok()?.trim().parse().ok()
    }
    /// tmp+rename for the same reason as `set_lineage`: a torn number would parse as garbage and
    /// read as `None`, which is the safe direction, but a tmp file left behind is one to clean.
    pub fn record_pushed_gen(&self, name: &str, generation: u64) -> Result<(), String> {
        let dst = self.pushed_gen_path(name);
        let tmp = self.voldir(name).join(".pushed-gen.tmp");
        std::fs::write(&tmp, generation.to_string()).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &dst).map_err(|e| format!("{}: {e}", dst.display()))
    }
```

- [ ] **Step 4: The beat** (in `controller.rs`, after `expired_requests`)

```rust
/// What the timer's pushes say in `history`. They bypass `SnapshotRequest` on purpose: they are
/// the agent's housekeeping, not something anyone asked for, and a request object per five minutes
/// per person would be noise in the listings.
pub const HOME_PUSH_MESSAGE: &str = "home: periodic";

/// `WS_HOME_PUSH_SECS`, default 300: how often this node pushes the homes whose disk moved. An
/// unchanged home costs one `subvolume show` per beat.
pub fn home_push_interval() -> Duration {
    Duration::from_secs(std::env::var("WS_HOME_PUSH_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(300))
}

/// The homes on this node due for a push, decided from two per-volume numbers so the decision is
/// testable without a filesystem: `generation` is what btrfs says now (`None`: the subvolume is
/// absent or unreadable — skipped, never pushed blind), `pushed` is what was recorded after the
/// last push (`None`: never pushed — so a home that exists gets its first record on the next beat).
pub fn homes_to_push(
    volumes: &[Arc<crd::Volume>],
    generation: impl Fn(&str) -> Option<u64>,
    pushed: impl Fn(&str) -> Option<u64>,
) -> Vec<Arc<crd::Volume>> {
    volumes
        .iter()
        .filter(|v| crd::is_home_volume(v) && volume_is_ready(v) && v.metadata.deletion_timestamp.is_none())
        .filter(|v| {
            let id = v.name_any();
            match (generation(&id), pushed(&id)) {
                (None, _) => false,
                (Some(now), Some(then)) => now != then,
                (Some(_), None) => true,
            }
        })
        .cloned()
        .collect()
}

/// The timer (spec: "Replication, trigger 1"). Every home is pushed from the agent's own beat and
/// nothing else: inside a region there is one node per person, so no two nodes ever push one home.
fn spawn_home_push(ctx: Arc<Ctx>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(home_push_interval());
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let ctx = ctx.clone();
            // Its own OS thread: `push_env` blocks on the volume's `flock`, and `subvolume show`
            // is a process per home. Same rule as every btrfs operation in this file.
            if let Err(e) = tokio::task::spawn_blocking(move || home_push_beat(&ctx)).await {
                tracing::warn!(error = %e, "home push beat panicked; skipping it");
            }
        }
    });
}

fn home_push_beat(ctx: &Ctx) {
    let engine = &ctx.engine;
    if let Err(e) = engine.sync_pool() {
        tracing::warn!(error = %e, "home push: btrfs sync; skipping the beat");
        return;
    }
    let due = homes_to_push(&ctx.volumes.state(), |id| engine.generation(id).ok(), |id| engine.pool.pushed_gen(id));
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::warn!(error = %e, "home push: runtime");
            return;
        }
    };
    for v in due {
        let id = v.name_any();
        // Not under a reconcile-owned operation on this volume (a materialize, a restore): the
        // `running` map is the single-flight guard for those, and the flock would only make this
        // beat wait for them anyway. A `SnapshotRequest` push (the stop) is keyed by its own uid
        // and is serialised by the flock; a beat right behind it pushes an identical tree once.
        // ponytail: that duplicate is one extra record per stop-then-beat coincidence; comparing
        // the generation again after the flock is the fix if `history` ever looks noisy.
        if running_contains(ctx, &v.uid().unwrap_or_default()) {
            continue;
        }
        match rt.block_on(engine.push_env(&v.spec.owner, &id, &serde_json::Value::Null, Some(HOME_PUSH_MESSAGE))) {
            Ok(_) => {
                // Read AFTER the push, never before: the snapshot's own transaction can move the
                // generation, and recording the earlier number makes every beat push again.
                // Writes that land between the snapshot and this read go out on the next beat.
                match engine.generation(&id) {
                    Ok(g) => {
                        if let Err(e) = engine.pool.record_pushed_gen(&id, g) {
                            tracing::warn!(volume = %id, error = %e, "home push: recording the generation");
                        }
                    }
                    Err(e) => tracing::warn!(volume = %id, error = %e, "home push: generation after push"),
                }
                metrics::counter!("home_pushes_total", "result" => "ok").increment(1);
            }
            // Logged and retried next beat; the subvolume is untouched (spec: failure modes).
            Err(e) => {
                tracing::warn!(volume = %id, error = %e, "home push failed; retrying next beat");
                metrics::counter!("home_pushes_total", "result" => "error").increment(1);
            }
        }
    }
}
```

`volume_is_ready` (line ~1148) becomes `pub(crate) fn volume_is_ready`. In `run`, right after `spawn_heartbeat(ctx.clone());` add `spawn_home_push(ctx.clone());`.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p rustic-git-workspaces --test engine_pool && cargo test -p rustic-git-agent && cargo clippy -p rustic-git-workspaces -p rustic-git-agent --all-targets -- -D warnings`
Expected: PASS, clippy clean.

- [ ] **Step 6: Commit**

```bash
git add crates/workspaces/src/engine/pool.rs crates/workspaces/tests/engine_pool.rs bins/agent/src/controller.rs bins/agent/tests/reconcile.rs
git commit -m "Push changed homes from an agent beat every five minutes"
```

---

### Task 7: A workspace stop pushes the home before the pod goes

**Files:**
- Modify: `bins/agent/src/controller.rs:2273-2350` (`await_stop_push` → `stop_push` + `StopPush`), `:1966-1988` (the environment's call site), `:1610-1640` (the workspace `Stopped` arm), `:311-330` (the workspace controller's watches in `run`)
- Modify: `deploy/k3s/agent-rbac.yaml` (table rows for `snapshotrequests create/delete`)
- Test: `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Consumes: `crd::home_volume_name` (Task 1), `Ctx::volumes`, `volume_is_ready` (Task 6), `crd::snapshot_request`, `crd::STOP_LABEL`, `owned_by`.
- Produces:
  ```rust
  pub(crate) enum StopPush { Landed, Failed, Waiting }
  /// One fixed-name request per (parent, volume); `Waiting` after creating it.
  async fn stop_push<P: Resource<DynamicType = ()> + ResourceExt>(name: &str, owner: &str, volume: &str, parent: &P, ctx: &Arc<Ctx>) -> Result<StopPush, ReconcileErr>;
  ```
  The environment behaviour is unchanged (its five existing stop tests stay green as-is).

- [ ] **Step 1: Write the failing tests** (in `reconcile.rs`, after the environment stop tests)

```rust
const WS_STOP_REQ: &str = "/apis/rustic-git.io/v1alpha1/snapshotrequests/stop-home-ws-1";
const WS_POD_DEL: &str = "/api/v1/namespaces/ws-alice/pods/ws-1";
const SNAPSHOTS: &str = "/apis/rustic-git.io/v1alpha1/snapshotrequests";

fn stopping_ws() -> crd::Workspace {
    let mut o = ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a", "compatibleNodes": ["node-a"],
                                            "volumeRef": "ws-1", "podRef": "ws-alice/ws-1"}));
    o["spec"]["desiredState"] = serde_json::json!("stopped");
    serde_json::from_value(o).unwrap()
}

fn home_stop_req(status: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "SnapshotRequest",
        "metadata": {"name": "stop-home-ws-1", "uid": "stop-home-uid-1"},
        "spec": {"volume": "home-alice"},
        "status": status,
    })
}

fn ws_stop_routes(req: Option<serde_json::Value>) -> Vec<Route> {
    let mut routes = vec![Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) }];
    match req {
        Some(r) => routes.push(rustic_git_workspaces::kube_test::get(WS_STOP_REQ, r)),
        None => routes.push(rustic_git_workspaces::kube_test::not_found(WS_STOP_REQ)),
    }
    routes
}

/// Trigger 2 of the spec: the home is pushed BEFORE the pod goes, through a `stop-home-{ws}`
/// request owned by the workspace, and nothing is deleted on the pass that creates it.
#[tokio::test]
async fn a_stopping_workspace_requests_a_home_push_before_deleting_its_pod() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = ws_stop_routes(None);
    routes.push(rustic_git_workspaces::kube_test::post(SNAPSHOTS, home_stop_req(serde_json::json!({"phase": "pending"}))));
    let (ctx, rec) = ctx(tmp.path(), routes);
    ctx.remember_volume(serde_json::from_value(home_vol_json(2)).unwrap());

    let action = rustic_git_agent::controller::apply_workspace(&stopping_ws(), &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)));
    let req = rec.sent("POST", SNAPSHOTS).remove(0);
    assert_eq!(req["metadata"]["name"], "stop-home-ws-1");
    assert_eq!(req["spec"]["volume"], "home-alice");
    assert_eq!(req["metadata"]["ownerReferences"][0]["kind"], "Workspace");
    assert_eq!(req["metadata"]["labels"][crd::STOP_LABEL], "ws-1", "the label the stop-request watch selects on");
    assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE")), "nothing landed yet: {:?}", rec.calls());
    let st = rec.sent("PATCH", WS_STATUS).last().cloned().unwrap();
    assert_eq!(st["status"]["phase"], "ready", "still running while the push is in flight");
    assert!(st["status"]["observedGeneration"].is_null());
}

/// Fail-closed: a failed home push keeps the pod. A stop that tore the pod down anyway would be
/// the one moment the person's dotfiles are lost for good.
#[tokio::test]
async fn a_failed_home_push_keeps_the_workspace_pod() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), ws_stop_routes(Some(home_stop_req(serde_json::json!({"phase": "error"})))));
    ctx.remember_volume(serde_json::from_value(home_vol_json(2)).unwrap());

    let action = rustic_git_agent::controller::apply_workspace(&stopping_ws(), &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "woken by the request's own status");
    assert!(!rec.calls().iter().any(|c| c.starts_with("DELETE")), "{:?}", rec.calls());
    let st = rec.sent("PATCH", WS_STATUS).last().cloned().unwrap();
    assert_eq!(st["status"]["phase"], "ready");
    assert!(
        st["status"]["conditions"].as_array().unwrap().iter()
            .any(|c| c["type"] == "Ready" && c["status"] == "False" && c["reason"] == "StopSnapshotFailed"),
        "{st}"
    );
}

/// The happy path: `done` deletes the pod AND the request, so the next stop creates a fresh one
/// instead of finding this `done` object under the same fixed name and stopping without a push.
#[tokio::test]
async fn a_landed_home_push_deletes_the_pod_and_its_request() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = ws_stop_routes(Some(home_stop_req(serde_json::json!({"phase": "done", "snapshotId": "layer-1"}))));
    routes.push(Route { method: "DELETE", path: WS_POD_DEL.into(), status: 200, body: serde_json::json!({"kind": "Status"}) });
    routes.push(Route { method: "DELETE", path: WS_STOP_REQ.into(), status: 200, body: home_stop_req(serde_json::json!({"phase": "done"})) });
    let (ctx, rec) = ctx(tmp.path(), routes);
    ctx.remember_volume(serde_json::from_value(home_vol_json(2)).unwrap());

    let action = rustic_git_agent::controller::apply_workspace(&stopping_ws(), &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change());
    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {WS_POD_DEL}")), "{:?}", rec.calls());
    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {WS_STOP_REQ}")), "the request must not outlive the stop: {:?}", rec.calls());
    let st = rec.sent("PATCH", WS_STATUS).last().cloned().unwrap();
    assert_eq!(st["status"]["phase"], "stopped");
    assert_eq!(st["status"]["observedGeneration"], 1);
}

/// An owner bound before homes existed has no home Volume on this node and nothing to lose: the
/// stop is the plain pod delete it always was. No request, no wait.
#[tokio::test]
async fn a_workspace_without_a_home_here_stops_without_a_push() {
    let tmp = tempfile::tempdir().unwrap();
    let mut routes = ws_stop_routes(None);
    routes.push(Route { method: "DELETE", path: WS_POD_DEL.into(), status: 200, body: serde_json::json!({"kind": "Status"}) });
    let (ctx, rec) = ctx(tmp.path(), routes);

    let action = rustic_git_agent::controller::apply_workspace(&stopping_ws(), &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change());
    assert!(rec.sent("POST", SNAPSHOTS).is_empty(), "{:?}", rec.calls());
    assert!(rec.calls().iter().any(|c| c == &format!("DELETE {WS_POD_DEL}")), "{:?}", rec.calls());
}

/// Same guard the environment has, now that the request is deleted after teardown: a workspace
/// already stopped at this generation must not create a fresh request on every later event.
#[tokio::test]
async fn an_already_stopped_workspace_pushes_nothing_again() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![]);
    ctx.remember_volume(serde_json::from_value(home_vol_json(2)).unwrap());
    let mut w = stopping_ws();
    let mut st = w.status.clone().unwrap();
    st.phase = crd::Phase::Stopped;
    st.observed_generation = Some(1);
    st.pod_ref = None;
    w.status = Some(st);

    let action = rustic_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change());
    assert!(rec.calls().is_empty(), "nothing to do: {:?}", rec.calls());
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p rustic-git-agent --test reconcile home_push; cargo test -p rustic-git-agent --test reconcile stopped_workspace`
Expected: `a_stopping_workspace_requests_a_home_push_before_deleting_its_pod` FAILS (the pod DELETE happens with no POST; the mock 404s the delete → the reconcile errors or records a DELETE), `a_failed_home_push_keeps_the_workspace_pod` FAILS (a DELETE is attempted), `an_already_stopped_workspace_pushes_nothing_again` FAILS (a DELETE is attempted).

- [ ] **Step 3: Generalise `await_stop_push` into `stop_push`**

Replace `await_stop_push` (keep its doc comment; it describes exactly this) with:

```rust
/// What a fixed-name stop request says about its push: landed, failed, or still to wait for
/// (including "just created it"). The caller writes ITS OWN status — the two parent kinds share
/// no status type, and this is the one place the request's lifecycle is decided.
pub(crate) enum StopPush {
    Landed,
    Failed,
    Waiting,
}

async fn stop_push<P>(name: &str, owner: &str, volume: &str, parent: &P, ctx: &Arc<Ctx>) -> Result<StopPush, ReconcileErr>
where
    P: Resource<DynamicType = ()> + ResourceExt,
{
    let api: Api<crd::SnapshotRequest> = Api::all(ctx.client.clone());
    // A request being deleted is ABSENT. The teardown deletes this object, and a `done` one that is
    // still terminating (a finalizer holds it) would otherwise read as a landed push for the NEXT
    // stop — tearing that one down without pushing at all.
    let req = api.get_opt(name).await?.filter(|r| r.metadata.deletion_timestamp.is_none());
    let mut phase = req.as_ref().map(|r| r.status.as_ref().map(|s| s.phase).unwrap_or(crd::Phase::Pending));
    let restarted = req
        .as_ref()
        .and_then(|r| r.status.as_ref())
        .is_some_and(|s| s.phase == crd::Phase::Error && s.conditions.iter().any(|c| c.reason == "AgentRestarted"));
    if restarted {
        delete_ignoring_404(&api, name).await?;
        phase = None;
    }
    match phase {
        Some(crd::Phase::Done) => Ok(StopPush::Landed),
        Some(crd::Phase::Error) => Ok(StopPush::Failed),
        Some(_) => Ok(StopPush::Waiting),
        None => {
            let mut req = crd::snapshot_request(name, owner, volume, Some("stopping".into()));
            // Owned by the parent so the request's own events map back to it — that watch is what
            // wakes the `Failed` arm. NOT a cascade-delete convenience: the request is deleted
            // explicitly after teardown.
            req.metadata.owner_references = Some(vec![owner_ref_of_kind(parent)?]);
            // The label both parent controllers select their request watch by. A view, like every
            // label here — the ownerReference above is what the mapper actually reads.
            req.metadata.labels.get_or_insert_with(Default::default).insert(crd::STOP_LABEL.to_string(), parent.name_any());
            match api.create(&PostParams::default(), &req).await {
                Ok(_) => {}
                Err(kube::Error::Api(s)) if s.code == 409 => {}
                Err(err) => return Err(err.into()),
            }
            Ok(StopPush::Waiting)
        }
    }
}
```

The environment's call site (`if let Some(action) = await_stop_push(&vol, e, gen, ctx).await? { return Ok(action); }`) becomes:

```rust
        match stop_push(&format!("stop-{}", e.name_any()), &e.spec.owner, &vol.name_any(), e, ctx).await? {
            StopPush::Landed => {}
            StopPush::Failed => {
                let st = crd::EnvironmentStatus {
                    phase: crd::Phase::Running,
                    observed_generation: None,
                    service_status: vec![],
                    conditions: vec![crd::condition(
                        "Ready",
                        false,
                        "StopSnapshotFailed",
                        "the stop snapshot failed; the services are kept (scaled to zero) rather than lose their state",
                        gen,
                    )],
                    ..e.status.clone().unwrap_or_default()
                };
                write_env_status(e, st, ctx).await?;
                return Ok(Action::await_change());
            }
            StopPush::Waiting => {
                let st = crd::EnvironmentStatus {
                    phase: crd::Phase::Running,
                    observed_generation: None,
                    service_status: vec![],
                    conditions: vec![crd::condition("Progressing", true, "PushBeforeStop", "waiting for the volume's push", gen)],
                    ..e.status.clone().unwrap_or_default()
                };
                write_env_status(e, st, ctx).await?;
                return Ok(Action::requeue(TICK));
            }
        }
```

(These two status bodies are moved verbatim out of the old function; the `// Still running: …` comments move with them.)

- [ ] **Step 4: The workspace `Stopped` arm**

Replace the arm at the top of `apply_workspace`:

```rust
    if w.spec.desired_state == DesiredState::Stopped {
        // Already stopped: nothing to do. Load-bearing now that `stop-home-{ws}` is DELETED after
        // the teardown — without this the request's absence reads as "no push yet" on every later
        // event, and a stopped workspace would push its home forever.
        if prev.phase == crd::Phase::Stopped {
            if prev.observed_generation != Some(gen) {
                let st = crd::WorkspaceStatus { observed_generation: Some(gen), ..prev };
                write_ws_status(w, st, ctx).await?;
            }
            return Ok(Action::await_change());
        }
        let ns = crd::ws_namespace(&w.spec.owner, &w.spec.team);
        // The Volume child takes the parent's own name, so the pod's name is known without reading
        // (or creating) it.
        let id = prev.volume_ref.clone().unwrap_or_else(|| w.name_any());
        // The home is pushed BEFORE the pod goes, and the pod goes only once the push has LANDED —
        // the same fail-closed gate an environment's own subvolume gets, and for the same reason: a
        // stop that tore the pod down on a failed push is the one moment the person's dotfiles are
        // lost for good. Gated on the home being on this node at all: an owner bound before homes
        // existed has none, and nothing to lose.
        let home = crd::home_volume_name(&w.spec.owner);
        let home_here = ctx
            .volumes
            .get(&kube::runtime::reflector::ObjectRef::new(&home))
            .is_some_and(|v| volume_is_ready(&v));
        let request = format!("stop-home-{id}");
        if home_here {
            match stop_push(&request, &w.spec.owner, &home, w, ctx).await? {
                StopPush::Landed => {}
                StopPush::Failed => {
                    let st = crd::WorkspaceStatus {
                        observed_generation: None,
                        conditions: ws_conditions(
                            &prev,
                            crd::condition(
                                "Ready",
                                false,
                                "StopSnapshotFailed",
                                "the home push failed; the pod is kept rather than lose the home's last state",
                                gen,
                            ),
                        ),
                        ..prev
                    };
                    write_ws_status(w, st, ctx).await?;
                    return Ok(Action::await_change());
                }
                StopPush::Waiting => {
                    let st = crd::WorkspaceStatus {
                        observed_generation: None,
                        conditions: ws_conditions(
                            &prev,
                            crd::condition("Progressing", true, "PushBeforeStop", "waiting for the home's push", gen),
                        ),
                        ..prev
                    };
                    write_ws_status(w, st, ctx).await?;
                    return Ok(Action::requeue(TICK));
                }
            }
        }
        delete_ignoring_404(&Api::<Pod>::namespaced(ctx.client.clone(), &ns), &id).await?;
        if home_here {
            // Served its purpose; left behind, the NEXT stop would find `done` under the same name
            // and stop without pushing at all.
            delete_ignoring_404(&Api::<crd::SnapshotRequest>::all(ctx.client.clone()), &request).await?;
        }
        // `ws_conditions`, not a bare vec: a stop that dropped `PackagesReady` left the web
        // showing "installing packages…" for a workspace that is simply off.
        let conditions = ws_conditions(&prev, crd::condition("Ready", true, "Converged", "workspace matches spec", gen));
        let st = crd::WorkspaceStatus {
            phase: crd::Phase::Stopped,
            observed_generation: Some(gen),
            volume_ref: Some(id),
            pod_ref: None,
            conditions,
            ..prev
        };
        write_ws_status(w, st, ctx).await?;
        return Ok(Action::await_change());
    }
```

Note the pod is NOT drained first: the spec gates the pod delete on the push, and a running shell's home is exactly what the timer already snapshots every five minutes — the stop push is the last of those, not a quiesced one.

- [ ] **Step 5: Wake the workspace on its stop request**

In `run`, the workspace controller gains one more watch (after `.watches_shared_stream(vol_ws, …)`):

```rust
        // The `stop-home-{ws}` request the stop path waits on, selected by the same `stop-of`
        // label the environments use and mapped back by ownerReference — without it a workspace
        // parked at `StopSnapshotFailed` would never wake, not even for an operator deleting the
        // failed request.
        .watches(
            Api::<crd::SnapshotRequest>::all(ctx.client.clone()),
            watcher::Config::default().labels(crd::STOP_LABEL),
            |r| owned_by::<crd::Workspace, _>(&r),
        )
```

- [ ] **Step 6: RBAC table rows** (`deploy/k3s/agent-rbac.yaml`, comment only — the verbs are already granted):

```
#   snapshotrequests (rustic-git.io)       get,list,watch               Controller; stop_push
#                                          create                       stop_push (stop-{env},
#                                                                       stop-home-{ws})
#                                          patch                        finalizer (metadata)
#                                          delete                       stop_push (AgentRestarted),
#                                                                       both stop teardowns;
#                                                                       spawn_snapshot_gc (old done)
```

and the rule comment on `snapshotrequests` gains ", and the same for `stop-home-{ws}` on a workspace stop".

- [ ] **Step 7: Run the tests**

Run: `cargo test -p rustic-git-agent && cargo clippy -p rustic-git-agent --all-targets -- -D warnings`
Expected: PASS — the five new workspace tests, and the five pre-existing environment stop tests unchanged.

- [ ] **Step 8: Commit**

```bash
git add bins/agent/src/controller.rs bins/agent/tests/reconcile.rs deploy/k3s/agent-rbac.yaml
git commit -m "Push the home before a workspace stop deletes its pod"
```

---

### Task 8: A raised `homeQuotaGb` reaches the home Volume

**Files:**
- Modify: `bins/agent/src/binding.rs` (`ensure_home` from Task 2)
- Modify: `deploy/k3s/agent-admission.yaml:34-40` (the Volume expression), `deploy/k3s/agent-rbac.yaml` (`volumes patch` row), `crates/workspaces/src/crd.rs:11-15` (module doc's list of spec exceptions)
- Test: `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Consumes: `ensure_home` (Task 2), `Engine::set_quota` (already runs on every Volume reconcile of a new generation — a spec patch IS a new generation, so no controller change).
- Produces: `ensure_home` merge-patches `spec.quotaGb` on the home Volume when it differs from `b.spec.home_quota_gb`.

- [ ] **Step 1: Write the failing test**

```rust
/// The binding carries the wish and the Volume is the agent's own object, so a changed quota is
/// copied down as ONE spec field — the second the admission policy allows by name, next to
/// `restoreTo`. `set_quota` then runs on the Volume's next pass, because a spec edit is a new
/// generation. An unchanged quota writes nothing.
#[tokio::test]
async fn a_raised_home_quota_is_copied_onto_the_home_volume_once() {
    let tmp = tempfile::tempdir().unwrap();
    let ws_list = serde_json::json!({
        "apiVersion": "rustic-git.io/v1alpha1", "kind": "WorkspaceList", "metadata": {},
        "items": [ws_json(serde_json::json!({"phase": "ready", "nodeName": "node-a"}))]
    });
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/workspaces", ws_list),
            Route { method: "PATCH", path: binding_status(), status: 200, body: binding_json() },
            rustic_git_workspaces::kube_test::get(HOME_VOL_GET, home_vol_json(2)),
            Route { method: "PATCH", path: HOME_VOL_GET.into(), status: 200, body: home_vol_json(5) },
            pv_route("home-ws-alice"),
            pvc_route("home"),
        ]
        .into_iter()
        .chain(ns_routes("ws-alice"))
        .collect(),
    );
    let mut b = binding_json();
    b["spec"]["homeQuotaGb"] = serde_json::json!(5);
    let b: crd::OwnerBinding = serde_json::from_value(b).unwrap();

    rustic_git_agent::binding::apply_binding(&b, &ctx).await.unwrap();

    let patch = rec.sent("PATCH", HOME_VOL_GET);
    assert_eq!(patch.len(), 1, "{:?}", rec.calls());
    assert_eq!(patch[0], serde_json::json!({"spec": {"quotaGb": 5}}), "quotaGb and nothing else");
    let pv = rec.sent("PATCH", "/api/v1/persistentvolumes/home-ws-alice");
    assert_eq!(pv[0]["spec"]["capacity"]["storage"], "2Gi", "the claim's number is nominal and must never change: the qgroup is the cap");

    // Same quota as the Volume already has: no spec write at all.
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            rustic_git_workspaces::kube_test::get("/apis/rustic-git.io/v1alpha1/workspaces", serde_json::json!({
                "apiVersion": "rustic-git.io/v1alpha1", "kind": "WorkspaceList", "metadata": {}, "items": []})),
            Route { method: "PATCH", path: binding_status(), status: 200, body: binding_json() },
            rustic_git_workspaces::kube_test::get(HOME_VOL_GET, home_vol_json(2)),
            pv_route("home-ws-alice"),
            pvc_route("home"),
        ]
        .into_iter()
        .chain(ns_routes("ws-alice"))
        .collect(),
    );
    let b: crd::OwnerBinding = serde_json::from_value(binding_json()).unwrap();
    rustic_git_agent::binding::apply_binding(&b, &ctx).await.unwrap();
    assert!(rec.sent("PATCH", HOME_VOL_GET).is_empty(), "{:?}", rec.calls());
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p rustic-git-agent --test reconcile a_raised_home_quota`
Expected: FAIL — `rec.sent("PATCH", HOME_VOL_GET)` is empty.

- [ ] **Step 3: Implement**

In `binding.rs` `ensure_home`, keep the Volume that `ensure_child_volume` returns and patch it:

```rust
    let vol = ensure_child_volume(&id, b, owner, "", &b.spec.region, &storage, &b.spec.node_name, "home", ctx).await?;
    if vol.spec.quota_gb != b.spec.home_quota_gb {
        // The ONE spec field this reconciler writes on its child, allowed by name in
        // agent-admission.yaml for Volumes an OwnerBinding owns: the binding carries the wish, the
        // Volume is the agent's own object, and the qgroup limit follows on the Volume's next pass
        // because a spec change is a new generation. A merge patch of that field alone — never a
        // re-apply of the whole spec, which would revert anything else.
        let api: Api<crd::Volume> = Api::all(ctx.client.clone());
        let patch = serde_json::json!({"spec": {"quotaGb": b.spec.home_quota_gb}});
        api.patch(&id, &PatchParams::default(), &Patch::Merge(&patch)).await?;
    }
```

Add `use kube::api::{Api, ListParams, Patch, PatchParams};` to the imports.

In `deploy/k3s/agent-admission.yaml`, the Volume branch of the first policy becomes:

```yaml
    - expression: >-
        object.kind == 'Volume'
          ? (object.spec.all(k, k == 'restoreTo'
                                || (k == 'quotaGb'
                                    && has(object.metadata.ownerReferences)
                                    && object.metadata.ownerReferences.exists(r, r.kind == 'OwnerBinding'))
                                || (k in oldObject.spec && object.spec[k] == oldObject.spec[k]))
             && oldObject.spec.all(k, k == 'restoreTo' || k in object.spec))
          : object.spec == oldObject.spec
      message: "rustic-git-agent writes status, not spec (the exceptions: Volume.spec.restoreTo, and Volume.spec.quotaGb on an OwnerBinding's home volume)"
```

and its header comment's "ONE spec field" paragraph gains: "…and, since the persistent home, `Volume.spec.quotaGb` on a Volume an `OwnerBinding` owns — `binding::ensure_home` copies the binding's `homeQuotaGb` down; the ownerReference is what scopes it, so a workspace's volume quota stays `/v1`'s."

`deploy/k3s/agent-rbac.yaml` `volumes patch` row: `finalizer (metadata), restore_gate (spec.restoreTo), binding::ensure_home (spec.quotaGb on home-{owner}) — the TWO spec fields, see the policy`. `crd.rs` line 13-15 (module doc): "(for labels, finalizers, `VolumeSpec::restore_to` and a home volume's `quota_gb`)".

- [ ] **Step 4: Run the tests**

Run: `cargo test -p rustic-git-agent --test reconcile binding && cargo clippy -p rustic-git-agent --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add bins/agent/src/binding.rs bins/agent/tests/reconcile.rs deploy/k3s/agent-admission.yaml deploy/k3s/agent-rbac.yaml crates/workspaces/src/crd.rs
git commit -m "Copy a binding's home quota onto its home volume"
```

---

### Not a task: home usage in the workspace list

The spec says the workspace list shows the home's usage "next to the workspace's own" and that `status.usageBytes` already exists on `Volume`. Neither is true in the code: `VolumeStatus` (`crd.rs:159-192`) has no usage field, `usageBytes`/`usage_bytes` occurs nowhere under `crates/`, `bins/` or `web/apps/web`, and the workspace list shows no usage for the workspace's own volume either. Showing the home's usage is therefore a new vertical — `btrfs qgroup show` on the node → a `VolumeStatus` field written by the Volume reconciler (with the no-op write guard `status_eq` extended) → `/v1` projection → `web/apps/web` — for both volume kinds, and it gets its own spec. Quota is still ENFORCED (Tasks 1, 2, 8 and the existing `set_quota`); it is the display that is deferred. The e2e in Task 10 asserts the enforcement, not a display.

---

### Task 9: End-to-end: two pods share the home, a stop pushes it, a restart keeps it

**Files:**
- Modify: `tests/ws_e2e.sh` (a new phase after the `kl ws ssh` phase, i.e. after the line `kubectl -n "$WS_NS" exec "$CLONE_ID" -- jq --version …`, and the final `OK:` line)

**Interfaces:**
- Consumes: the running `WS_ID` and `CLONE_ID` pods (both on `$E2E_NODE`), `USER_NAME`, `WS_NS`, `live_dir`, `id_count`, `wait_ws_ready`, `POST /v1/workspaces/{id}/stop|start`, `GET /v1/volumes/{name}/history` (reads the server registry, so it sees timer pushes and stop pushes alike).

- [ ] **Step 1: Add the phase** (this script cannot run on this Mac — read it twice; exit 77 semantics unchanged)

```bash
# ---------------------------------------------------------------------------
# Persistent home: everything under /home/kl except ~/workspace is ONE btrfs subvolume per person
# per node (`home-{owner}`), mounted into every workspace pod of theirs, pushed on a timer and on
# every workspace stop. Two pods on one node see the same file at once — a local fact, no push
# involved — and the stop is what makes the registry copy. Both WS_ID and CLONE_ID are on this
# node (the binding pins the owner here), which is what makes the second read non-vacuous.
# ---------------------------------------------------------------------------
HOME_VOL="home-$(echo "$USER_NAME" | tr '[:upper:]' '[:lower:]')"
log "checking the home volume is Ready, claimed, and carries its nested cache subvolumes"
kubectl wait --for=condition=Ready "volume/$HOME_VOL" --timeout=120s || fail "home volume $HOME_VOL never became Ready"
kubectl get "volume/$HOME_VOL" -o jsonpath='{.metadata.ownerReferences[0].kind}' | grep -q OwnerBinding \
  || fail "the home volume is not owned by the OwnerBinding"
kubectl -n "$WS_NS" get pvc/home -o jsonpath='{.status.phase}' | grep -q Bound || fail "home claim in $WS_NS is not Bound"
for d in .cache .npm .cargo/registry .local/share/pnpm; do
  sudo test -d "$(live_dir "$HOME_VOL")/$d" || fail "home is missing its nested subvolume $d"
done
# inode 256 is a subvolume root; a plain directory the prelude might have made is not.
[ "$(sudo stat -c %i "$(live_dir "$HOME_VOL")/.cache")" = "256" ] || fail ".cache is a plain directory, not a nested subvolume"

log "writing ~/.zshrc in one workspace and reading it from the other"
kubectl -n "$WS_NS" exec "$WS_ID" -- sh -c "echo 'export WS_E2E_HOME=1' >> /home/kl/.zshrc" \
  || fail "could not append to /home/kl/.zshrc in $WS_ID"
kubectl -n "$WS_NS" exec "$CLONE_ID" -- grep -q 'WS_E2E_HOME=1' /home/kl/.zshrc \
  || fail "a second workspace on the same node does not see the home's .zshrc"
# As the login user, not root: the nested subvolumes are made by the agent (root) and chowned.
kubectl -n "$WS_NS" exec "$WS_ID" -- su kl -s /bin/sh -c 'touch /home/kl/.cache/e2e && touch /home/kl/.cargo/registry/e2e' \
  || fail "the nested cache subvolumes are not writable by kl"

log "stopping the workspace: the home is pushed before the pod goes"
HOME_BEFORE=$(id_count "$(curl -fsS "$BASE/v1/volumes/$HOME_VOL/history" -H "Authorization: Bearer $USER_TOKEN")")
curl -fsS -X POST "$BASE/v1/workspaces/$WS_ID/stop" -H "Authorization: Bearer $USER_TOKEN" >/dev/null
kubectl wait --for=jsonpath='{.status.phase}'=stopped "workspace/$WS_ID" --timeout=300s \
  || fail "workspace $WS_ID never reached phase=stopped"
HOME_AFTER=$(id_count "$(curl -fsS "$BASE/v1/volumes/$HOME_VOL/history" -H "Authorization: Bearer $USER_TOKEN")")
[ "$HOME_AFTER" -gt "$HOME_BEFORE" ] || fail "stopping a workspace did not push its home ($HOME_BEFORE -> $HOME_AFTER)"
kubectl get "snapshotrequest/stop-home-$WS_ID" >/dev/null 2>&1 && fail "the stop-home request outlived the stop"
kubectl -n "$WS_NS" get "pod/$WS_ID" >/dev/null 2>&1 && fail "the pod is still there after the stop"

log "starting the workspace again: the rc line survives the pod, and the cache did not travel"
curl -fsS -X POST "$BASE/v1/workspaces/$WS_ID/start" -H "Authorization: Bearer $USER_TOKEN" >/dev/null
wait_ws_ready "$WS_ID"
kubectl -n "$WS_NS" wait --for=condition=Ready "pod/$WS_ID" --timeout=120s || fail "pod $WS_ID did not come back"
kubectl -n "$WS_NS" exec "$WS_ID" -- grep -q 'WS_E2E_HOME=1' /home/kl/.zshrc || fail "the home's .zshrc did not survive a stop/start"
# The push carried the rc file and not the cache: a nested subvolume is never in the send stream.
# Proven off the registry copy by restoring the newest home snapshot into a scratch workspace.
HOME_SNAP=$(curl -fsS "$BASE/v1/volumes/$HOME_VOL/history" -H "Authorization: Bearer $USER_TOKEN" \
  | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
[ -n "$HOME_SNAP" ] || fail "no snapshot id in $HOME_VOL history"
HOME_RESTORE_JSON=$(curl -fsS -X POST "$BASE/v1/workspaces/restore" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"name":"e2e-home-restore","snapshot_id":"'"$HOME_SNAP"'","src_workspace":"'"$HOME_VOL"'"}')
HOME_RESTORE_ID=$(echo "$HOME_RESTORE_JSON" | field id)
[ -n "$HOME_RESTORE_ID" ] || fail "no id in home restore response: $HOME_RESTORE_JSON"
wait_ws_ready "$HOME_RESTORE_ID"
grep -q 'WS_E2E_HOME=1' "$(live_dir "$HOME_RESTORE_ID")/.zshrc" || fail "the pushed home does not carry .zshrc"
sudo test -e "$(live_dir "$HOME_RESTORE_ID")/.cache/e2e" && fail "the pushed home carried the .cache contents"
curl -fsS -X DELETE "$BASE/v1/workspaces/$HOME_RESTORE_ID" -H "Authorization: Bearer $USER_TOKEN" >/dev/null
wait_ws_gone "$HOME_RESTORE_ID"
```

If `POST /v1/workspaces/restore` refuses `src_workspace` that is not a workspace id (check `restore_ws` in `crates/workspaces/src/api.rs` — it resolves the source through `volume_owner`, which is a registry lookup by `(owner, name)`, so `home-{owner}` resolves like any volume), keep the block; if it 404s on the VM, replace the four `HOME_RESTORE_*` lines with a direct check on the node: `sudo btrfs subvolume list "$MOUNT" | grep -q "recv/"` is not specific enough — instead delete the scratch and assert only the two `grep`/`test` lines against `$(live_dir "$HOME_VOL")` and note the restore check as a follow-up in the script comment.

Also add the phase's workspace ids to the cleanup trap's list (`HOME_RESTORE_ID` next to the other `*_ID` variables it deletes), and extend the final `OK:` line with `persistent home (shared by two pods, stop pushes it, start keeps it, caches excluded)`.

- [ ] **Step 2: Lint the script**

Run: `bash -n tests/ws_e2e.sh && (command -v shellcheck >/dev/null && shellcheck -S warning tests/ws_e2e.sh || true)`
Expected: `bash -n` prints nothing; shellcheck introduces no new warnings in the added block.

- [ ] **Step 3: Run it on the Linux VM** (documented in CLAUDE.md as the only place it runs)

Run: `cargo build --bin rustic-git --bin rustic-git-api --bin rustic-git-agent --bin kl && ./tests/ws_e2e.sh`
Expected: `OK: … persistent home (…) all passed`; exit 0. On this Mac: exit 77 with `SKIP: btrfs/mkfs.btrfs not on PATH`.

- [ ] **Step 4: Commit**

```bash
git add tests/ws_e2e.sh
git commit -m "Exercise the persistent home end to end"
```

---

### Task 10: Docs

**Files:**
- Modify: `CLAUDE.md` ("Workspaces and environments" section, after the paragraph that ends "…the one place push happens without an explicit `/push` call.")
- Modify: `README.md` ("Request flows", after the workspace paragraph around line 179-190; and the Node agent row in "Components")

- [ ] **Step 1: CLAUDE.md** — append this paragraph to "Workspaces and environments":

```markdown
**Every person has one persistent home per node** — `/home/kl` in every workspace pod of theirs
is the SAME btrfs subvolume, `{pool}/vol/home-{owner}/live`, with `~/workspace` (the workspace's
own subvolume) mounted inside it. It is a child `Volume` named `crd::home_volume_name(owner)`
authored by the OwnerBinding reconciler (`bins/agent/src/binding.rs`, `ensure_home`) with the
binding as owner, plus a local PV `home-{ns}` and a PVC `home` in each of the owner's namespaces;
registry name `vol/{owner}/home-{owner}`, nothing special-cased. Caches (`k8s::HOME_LOCAL_DIRS`)
are NESTED subvolumes — `btrfs send` skips them and the qgroup does not count them — recreated
after every materialize and restore (`Engine::ensure_home_dirs`). Two pushes, both `Engine::push_env`:
the agent beat every `WS_HOME_PUSH_SECS` (default 300, message `home: periodic`, no
`SnapshotRequest`) pushes homes whose btrfs generation moved past `{voldir}/.pushed-gen`, and a
workspace stop creates `stop-home-{ws}` and deletes the pod only once it is `done` — fail-closed
like `stop-{env}`, and a workspace whose owner has no home Volume on this node stops without one.
First materialization on a node with no subvolume pulls the registry's `main` if there is history
(`Engine::materialize_home`); an unreachable registry is `RegionUnreachable`, permanent, and
creates nothing. `homeQuotaGb` on the binding (default 2) is copied onto the home Volume's
`quotaGb` — the SECOND spec field the agent may write, allowed by ownerReference kind in
`agent-admission.yaml` — and enforced by the same qgroup limit as every volume. Cross-region: each
region has its own copy and nothing syncs them.
```

- [ ] **Step 2: README.md** — after the workspace flow paragraph in "Request flows":

```markdown
**Persistent home.** `/home/kl` in every workspace pod of a person on a node is one shared btrfs
subvolume (`home-{owner}`, a Volume owned by their `OwnerBinding`), with the workspace's own
subvolume mounted at `~/workspace` inside it. Dotfiles are seeded once and then theirs. The agent
pushes it every five minutes when it changed and before every workspace stop; a node that has
never seen the person pulls the region's copy. Package caches (`.cache`, `.npm`,
`.cargo/registry`, `.local/share/pnpm`) are nested subvolumes: never uploaded, never counted
against the 2 GB home quota. Regions do not sync homes with each other.
```

and the Node agent row's "Owns" cell gains "per-owner home volumes and their pushes".

- [ ] **Step 3: Commit**

```bash
git add CLAUDE.md README.md
git commit -m "Document the persistent per-person home"
```

---

## Self-review

**Spec coverage.**
- Objects (home Volume from the binding, ownerReference, owner/team/nodeName/quotaGb/source): Task 2; `homeQuotaGb` default 2: Task 1; delete through the finalizer: the existing `cleanup_volume` path, unchanged, since the home is an ordinary Volume child.
- Registry name `vol/{owner}/home-{owner}`: Task 1 (`home_volume_name`) — `push_env(owner, id)` keys by `(owner, id)`.
- Mounting (PV/PVC from the binding, RWO, `/home/kl` before `/home/kl/workspace`, `.ssh` Secret inside the PV, environments do not mount it): Tasks 2 and 4. Deviation (PV per namespace) stated in Global Constraints.
- First start and dotfiles (seed only if absent; `~/workspace` a mount point): Task 4 — the prelude already seeds only if absent; comment updated.
- Cache exclusion (`HOME_LOCAL_DIRS`, created at materialize, recreated on restore, escape hatch): Task 3.
- Replication trigger 1 (timer, `WS_HOME_PUSH_SECS`, generation vs `.pushed-gen`, `home: periodic`, bypasses `SnapshotRequest`): Task 6. Trigger 2 (stop, `stop-home-{ws}`, gated on `done`, fail-closed): Task 7.
- Pull on first materialization, never over an existing subvolume, `REGION_UNREACHABLE` permanent and nothing created: Task 5.
- Concurrency (one node per person per region; no cross-region sync): design property, documented in Task 10; nothing to build.
- Quota (`homeQuotaGb` → `quotaGb`, qgroup, `ENOSPC`, no soft limit): Tasks 1, 2, 8 + existing `set_quota`. Usage display: not built, decided and stated ("Not a task").
- Failure modes table: PV missing → pod Pending, Task 2's doc comment; timer failure logged/retried → Task 6; stop failure keeps pod → Task 7; registry unreachable → Task 5; restore vs caches → Task 3; over quota → existing qgroup.
- Tests listed in the spec: crd/k8s unit (Tasks 1, 4), engine btrfs (Task 3), controller (Tasks 2, 5, 6, 7, 8), e2e (Task 9).

**Placeholder scan.** No "TBD", "TODO", "similar to", or "add tests" remain; the one conditional in Task 9 (if `restore` refuses a home id) names the exact replacement lines.

**Type consistency.** `ensure_child_volume(id: &str, parent, owner, team, region, storage, node, kind, ctx)` in Task 2 is what Task 8 calls; `home_vol_json(quota)` (Task 2) is used in Tasks 5, 6, 7, 8; `HOME_VOL_GET`/`VOLUMES` constants (Task 2) reused in Task 8; `volume_is_ready` made `pub(crate)` in Task 6 before Task 7 uses it in `apply_workspace` (same module — visibility is moot there, but `homes_to_push` is `pub` and calls it, hence the change); `StopPush`/`stop_push` (Task 7) match at both call sites; `Engine::generation`/`sync_pool`/`ensure_home_dirs` (Task 3) match Task 6's beat and the e2e's expectations; `Pool::pushed_gen`/`record_pushed_gen`/`pushed_gen_path` (Task 6) consistent between the test and the beat; `crd::STOP_LABEL`, `crd::snapshot_request`, `owned_by` are existing names used unchanged.
