# Controllers Own Their Children — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development
> (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move placement and child-object creation out of `/v1` and into the node agent, so the API
writes exactly one object per user action and every controller owns (and garbage-collects) the
children it authors.

**Architecture:** `bins/api` writes one `Workspace`/`Environment` with `storage` and no node. Each
agent runs a second watch on `status.nodeName=` (empty) and CLAIMS an object with a conditioned
status patch, then creates its `OwnerBinding`. The Workspace/Environment reconcilers create their
own `Volume` child (ownerReference), wait for `VolumeReady` + `NamespaceReady`, then make PV/PVC and
the pod. A push is no longer an annotation on the `Volume` — it is a new `SnapshotRequest` CR with
its own reconciler. `deploy/k3s/crds.yaml` is generated from the Rust types.

**Tech Stack:** Rust, `kube` 4.2.0 (`derive` + `runtime`), `k8s-openapi` 0.28.0 (`v1_33`),
`schemars` 1, `tokio`, `axum`, existing `Engine`/btrfs code (unchanged). Web: Next.js app router in
`web/apps/web`.

**Spec:** `docs/superpowers/specs/2026-08-27-controller-ownership-design.md` — read it alongside
this plan; it is the authority and this plan argues from it.

## Global Constraints

Copied verbatim from the spec (and from `CLAUDE.md`, which binds every task):

- "**The API writes what the user wants. Controllers make it happen, and own every object they
  make.**"
- "The API creates exactly one object per user action: a `Workspace` or an `Environment`, unplaced.
  It never names a node, never creates a `Volume`, never writes into a workspace namespace before
  that namespace exists."
- "A controller that creates a child stamps an `ownerReference`; the child dies with the parent."
- "Status flows up: a parent acts on a child only by reading the child's `status`, never by
  guessing."
- "**The cluster is the source of truth** for `Workspace`, `Environment`, `Volume`,
  `SnapshotRequest` and `OwnerBinding`. There is no copy in the API, none in Cosmos (which holds
  only `Region`), none in the web."
- "All five kinds stay cluster-scoped and keep `/status`. `Workspace` and `Environment` select on
  `.status.nodeName` … `Volume` and `OwnerBinding` keep `.spec.nodeName` (their spec is
  controller-written); `SnapshotRequest` has none."
- "The claim is a **status** write: controllers never touch an API-authored spec."
- "Otherwise claim = one **optimistic** status write: `replace_status` (or a non-forced
  server-side apply) carrying the object's current `metadata.resourceVersion` … A 409 means another
  node won; re-read and go to 1. A forced apply would never conflict and is therefore wrong here —
  this is the one write in the system that must race."
- "`spec: { volume, message? }` … Nothing else: a node is a controller-owned fact and the API does
  not copy facts into spec. Every agent watches all `SnapshotRequest`s (no field selector — two
  nodes today) and acts only when the named Volume's `spec.nodeName` is its own."
- "**Finalizer** `kloudlite-git.io/snapshot`: a delete while `working` must wait for the in-flight
  btrfs send / upload to finish … a request is removed only when nothing is running for it."
- "`Volume.status.lastPush`, which is **dropped**: 'the latest snapshot' is a query over
  `SnapshotRequest`s by volume label, not a second writer of the Volume's status (two controllers
  force-applying one status object under one field manager prune each other's fields)."
- "**Agent restart while `working`** … It is NOT re-run — a second `engine.push` would take a fresh
  snapshot and register a second commit record. It is marked `phase: error`, condition
  `Ready=False/AgentRestarted`; the user pushes again."
- "Errors are classified: a permanent one (unknown volume, volume on another node forever, invalid
  spec) writes the condition and `await_change()`; a transient one (registry 5xx, btrfs busy)
  requeues with backoff. The same rule applies to every reconciler in this design."
- "`phase` is a Rust enum on every kind so the generated schema carries `enum`. Every status carries
  `observedGeneration` and `conditions`; the no-op guards that ignore `lastTransitionTime` stay."
- "**Two-step schema change.** … Release 1: the fields stay in the schema as optional, the new
  `status` fields and `storage` block are added, agents migrate. Release 2 (after every node has
  rolled and every Workspace carries `status.volumeRef`): drop the two spec fields. There is no
  rollback across release 2."
- "Scope, decided 27 Aug: **two nodes for now — one session node, one env node.**"
- "`ponytail:` the claim checks no free space, and the placement algorithm moves into the agent
  unchanged (`placement.rs`), so a second session node is a deploy, not a code change."
- "Init container image for git seeding: pinned `alpine/git` (so seeding works with any workspace
  image); the pin lives in the agent's env like `WS_GIT_SSH_HOST`."
- "Objects it creates carry an ownerReference to the `OwnerBinding` … **except the namespace and
  LimitRange**, which keep no owner as today."
- "No change to the registry surface, the engine, or Cosmos."
- RBAC: "Agent: gains `create` on `volumes`, `snapshotrequests` (the Environment stop child) and
  `ownerbindings` (children it authors); `get/list/watch/patch/update` + `/status` + `/finalizers`
  on `snapshotrequests`; keeps `/status` on everything; keeps `patch` on `workspaces`/`environments`
  main resource ONLY for `heal_labels`." "API: loses `ownerbindings` and `volumes`
  `create`/`delete`; keeps `get/list` on volumes for projections; gains `create/get/list/delete` on
  `snapshotrequests`. `all_crds()` and `deploy/k3s/crds.yaml` gain the fifth kind."
- House style (CLAUDE.md): comments explain WHY, never what; deliberate shortcuts are marked
  `// ponytail: <ceiling and upgrade path>`; commit subjects are imperative sentence case with no
  tool attribution; `cargo clippy --workspace -- -D warnings` gates CI; `deploy/k3s/crds.yaml` is a
  GENERATED artifact checked by `crates/workspaces/tests/crd_yaml.rs`; `./tests/ws_e2e.sh` needs a
  Linux VM with btrfs + k3s and exits 77 on this Mac — it is a CI/VM step, never a local gate.

---

## Decisions (things the spec leaves open)

**D1. The child `Volume` keeps the SAME name as its parent.** `Volume::new(&w.name_any(), …)`. The
API already mints one id (`rid("ws")`) and uses it for both objects today
(`crates/workspaces/src/api.rs:486`, `:519-520`), and that id is also the registry key
(`vol/{owner}/{id}`), the PV name (`k8s::pv_name`), the PVC name, the pod name and the URL segment
in `/v1/volumes/{id}/history`. Giving the child a fresh name would move every one of those. The
ownerReference — not the name — is what makes it a child; the shared name is a convenience the rest
of the system already depends on.

**D2. `/v1/volumes/{id}/history` keeps the `CommitRecord` wire shape, and `lineage` goes empty.**
The web reads this array; `crates/workspaces/src/registry.rs:22-36` defines it as
`{id, state, lineage, region, message?, created_at}`. Listing `SnapshotRequest`s instead of calling
the registry produces the same JSON: `id` = `status.snapshotId`, `created_at` = `status.at`,
`message` = `spec.message`, `region` = the parent's region, `state` = `null`, `lineage` = `[]`.

`lineage` is the one field that genuinely loses information: it is layer bookkeeping that lives in
the record on the server tier, and copying it into etcd would put megabytes into an object the API
server lists. Exactly one place renders it —
`web/apps/web/src/app/(shell)/[owner]/(org)/snapshots/[id]/page.tsx:55-77`, which shows
"`N block + M stream`" and the last entry's `sha256`. **Task 9 removes that fragment in the same
task that empties the field**, per the user's rule: keep the wire shape the web reads, or change
both sides together. Every other reader is safe — `placement` is already typed
`string | null` and is never read anywhere in the web tier, and `CommitRecord.region`/`.state` are
never rendered.

**D2a. Why not keep calling the registry for `lineage`.** The spec is explicit: "The registry
`get_history` client call leaves `bins/api` entirely", and "No registry read on the request path."
Keeping one read just to decorate a row would re-add the cross-tier dependency the design deletes.

**D3. Only `phase: done` SnapshotRequests appear in `/history` and in `/refs`.** A pending or failed
request is a wish, not a snapshot; `refs.main` is the newest `done` one's `snapshotId`.

**D4. Superseded by D12** (SnapshotRequest naming), kept as a number so the later references stay
readable. See D12.

**D5. `Volume.status.lastPush` is DROPPED and nothing replaces it on the Volume.** The spec is
explicit: "the latest snapshot" is a query over `SnapshotRequest`s by volume label, not a second
writer of the Volume's status. Two controllers force-applying one status object under one field
manager prune each other's fields — the Volume reconciler's next `Patch::Apply(..).force()` would
delete any `lastSnapshot` the snapshot reconciler had written, because SSA removes fields a manager
previously owned and no longer sets. So `api::volume_ptr` (`api.rs:334-338`) becomes a list of
`done` SnapshotRequests by the `kloudlite-git.io/volume` label, and the snapshot reconciler writes
nothing outside its own object.

**D5a. Where "has this ever been pushed" is computed.** `list_ws` and `list_volumes` do ONE label
list of `done` SnapshotRequests for the owner and build a `HashSet<String>` of volume names, rather
than a list per row. Same shape as `volumes_of`'s existing single-call join (`api.rs:375-379`).

**D6. A node's ROLE comes from its own Node labels, not a new env var.** The agent reads the `Node`
object named `$NODE_NAME` at startup and runs the unplaced-Workspace watch iff it carries
`kloudlite-git.io/session=true`, the unplaced-Environment watch iff `kloudlite-git.io/env=true`. Those
labels already exist and already drive `k8s::placement`'s nodeSelector
(`crates/workspaces/src/k8s.rs:425-447`); a second hand-maintained copy in the DaemonSet env is a
second thing that can be wrong.

**D7. Init container image: `WS_GIT_INIT_IMAGE`, default `alpine/git:2.45.2`.** The spec's prose
("pinned `alpine/git`") and its inline code block ("image: `<the workspace image>`") disagree; the
prose is the decision and the reason is stated there — seeding must work with any workspace image.
A digest pin is not used because the DaemonSet yaml, not this code, is where image pins are reviewed.

**D8. `compatibleNodes` is NOT a selectable field.** Selectable fields may not be arrays (spec,
Objects §), so the only new selector is `.status.nodeName` on Workspace and Environment.

**D9. The migration's registry read uses `ctx.engine.registry`.** `Engine.registry` is a public
`RegistryClient` (`crates/workspaces/src/engine/ops.rs:147`), already pointed at the server tier by
`build_engine` (`bins/agent/src/lib.rs:68-75`). No new client, no new env.

**D10. `VolumeSpec.nodeName` stays, and stays the Volume's shard key.** Only the PARENTS lose
`spec.nodeName`. The Volume is controller-written, so its spec is legitimately the controller's.

**D11. `SnapshotRequest` has no `nodeName` and no field selector.** The spec: "a node is a
controller-owned fact and the API does not copy facts into spec." Every agent watches every
SnapshotRequest and acts only when `Volume(spec.volume).spec.nodeName == me`; a request whose Volume
is on another node returns `await_change()` and writes nothing — no condition, no phase, because
another agent owns it and two agents writing one status is the problem being avoided. A request
whose Volume does not exist YET is not an error either: it waits, woken by the
`SnapshotRequest`→`Volume` watch. `ponytail:` every agent sees every request; two nodes today, so
the fan-out is two. Add a `spec.volume`-indexed reflector if the request count ever makes this hot.

**D12. `SnapshotRequest` names for the E2E-visible cases.** `rid("snap")` for a user push,
`stop-{env_id}` (fixed) for an environment's stop child so "create if missing" is idempotent across
restarts, `snap-{record_id}` for a migration backfill so a re-run cannot mint a duplicate.
(This supersedes the earlier D4, which said only the first of the three.)

**D13. Error classification, one helper, used everywhere.** The audit's item 9: today's flat
`Action::requeue(RETRY)` makes a bad spec indistinguishable from a down registry. Task 2 introduces
`controller::Outcome` and every reconciler in Tasks 3-7 returns through it — permanent (a `cloneOf`
naming nothing, a `restoreOf` whose snapshot no `done` request carries, a Volume on another node
forever) writes the condition and `await_change()`; transient (API 5xx, registry unreachable, btrfs
busy) returns `Err` and takes `error_policy`'s backoff.

**D14. `phase` is a Rust enum on all five kinds.** So schemars emits `enum` and the API server
rejects a typo with a 422 rather than letting it reach the `/v1` projection, where an unknown string
silently becomes the default — the exact bug `controller.rs:694` documents. The existing
`phase_names_the_doc_enum` test (`bins/agent/tests/reconcile.rs:154-176`) becomes redundant for the
kinds it covers but stays, since it pins the WIRE strings against `model::WsState`/`EnvState`, which
are a different enum in a different crate.

**D15. Release 1 keeps `spec.nodeName` and `spec.volumeRef` as optional deprecated fields.** A CRD
apply is cluster-wide and pruning is irreversible, while agents roll per node: dropping them in the
same release that migrates them loses the Volume pointer for any object whose agent has not rolled.
Task 1 keeps them; Task 11 drops them, gated on evidence.

---

## File Map

| File | Responsibility after this plan |
|---|---|
| `crates/workspaces/src/crd.rs` | The five kinds. Gains `WorkspaceStorage`, `SnapshotRequest`, `Phase` enums, status `nodeName`/`compatibleNodes`/`volumeRef`; loses `Volume.status.lastPush` and `credential_secret` on `GitRepo`. `spec.nodeName`/`spec.volumeRef` stay optional and deprecated until Task 11. |
| `deploy/k3s/crds.yaml` | Generated from the above. Never hand-edited. |
| `crates/workspaces/tests/crd_yaml.rs` | Drift check + generator; gains the selectable-field-per-kind rule and the `phase` enum check. |
| `bins/agent/src/placement.rs` | **New** (moved from `crates/workspaces/src/placement.rs`): `place`, `pick`, `bytes` — unchanged algorithm. |
| `bins/agent/src/claim.rs` | **New**: the unplaced-object reconciler (compatibility rule, conditioned status patch, OwnerBinding create). |
| `bins/agent/src/binding.rs` | **New**: the `OwnerBinding` reconciler (namespaces, LimitRange, policies, api_secret_binding, `NamespaceReady`). |
| `bins/agent/src/snapshot.rs` | **New**: the `SnapshotRequest` reconciler. |
| `bins/agent/src/migrate.rs` | **New**: the one-shot startup migration. |
| `bins/agent/src/controller.rs` | `Ctx`, shared plumbing, and the Volume/Workspace/Environment reconcilers, rewritten. |
| `bins/agent/src/lib.rs` | Module list + `run` (unchanged otherwise). |
| `bins/agent/tests/reconcile.rs` | Stub-API-server tests for all of the above. |
| `crates/workspaces/src/api.rs` | `/v1` only: one object per action, projections from CRs, `SnapshotRequest`-backed push/history/refs. |
| `crates/workspaces/src/k8s.rs` | Gains `git_init_container`; `workspace_pod` takes the storage source. |
| `deploy/k3s/{agent,api}-rbac.yaml`, `deploy/k3s/agent-daemonset.yaml` | New verbs, new env, `WS_GIT_BASE` deleted. |
| `tests/ws_e2e.sh` | New git-seeding phase. |
| `web/apps/web/src/lib/api.ts` + workspace pages | Verified unchanged (D2). |

---

## Task 1: CRD surface (Release 1 — additive, nothing pruned)

**Files:**
- Modify: `crates/workspaces/src/crd.rs:47-68` (`VolumeSource`), `:70-77` (`LastPush`),
  `:111-130` (`VolumeStatus`), `:164-209` (`Workspace`), `:220-259` (`Environment`),
  `:261-292` (`OwnerBinding`), `:348-355` (`all_crds`)
- Modify: `deploy/k3s/crds.yaml` (regenerated, never hand-edited)
- Test: `crates/workspaces/tests/crd_yaml.rs:27-45`

**This is Release 1 of a two-step schema change (D15).** `spec.nodeName` and `spec.volumeRef` stay
in the parents' schemas as OPTIONAL, deprecated fields. Do not delete them here — a CRD apply is
cluster-wide and pruning is irreversible, while the agents roll per node, so an object whose
migration has not run yet would lose its only pointer to its Volume the first time anything writes
it. Task 11 drops them, gated on evidence that nothing needs them.

**Interfaces:**
- Produces: `crd::WorkspaceStorage { quota_gb: u64, source: Option<VolumeSource> }`;
  `crd::Phase` (`Pending | Creating | Ready | Running | Stopped | Working | Done | Error`);
  `crd::WorkspaceSpec { owner, team, name, region, image, storage, desired_state, resources, node_name: Option<String>, volume_ref: Option<String> }` (the last two deprecated);
  `crd::WorkspaceStatus { phase: Phase, observed_generation, node_name: String, compatible_nodes: Vec<String>, volume_ref: Option<String>, pod_ref, conditions }`;
  `crd::EnvironmentSpec { owner, name, region, services, storage, desired_state, node_name: Option<String>, volume_ref: Option<String> }`;
  `crd::EnvironmentStatus { phase: Phase, observed_generation, node_name, compatible_nodes, volume_ref, service_status, conditions }`;
  `crd::SnapshotRequest` / `SnapshotRequestSpec { volume: String, message: Option<String> }` (NO `node_name`) /
  `SnapshotRequestStatus { phase: Phase, observed_generation, snapshot_id, lineage_tip, at, conditions }`;
  `crd::VolumeStatus { phase: Phase, .., no last_push }`; `crd::VolumeSource::GitRepo { repo, branch }`;
  `crd::SNAPSHOT_FINALIZER`; `crd::VOLUME_LABEL`.
- Consumes: nothing.

- [ ] **Step 1: Write the failing test**

Append to `crates/workspaces/tests/crd_yaml.rs`, and replace the body of
`every_crd_has_a_status_subresource_and_a_nodename_field_selector` (currently lines 27-45) with:

```rust
#[test]
fn every_crd_has_a_status_subresource_and_the_right_node_selector() {
    // Both halves fail SILENTLY when dropped: without `status: {}` a status update folds into
    // spec (and the RBAC split becomes decorative); without `selectableFields` every node's
    // controller sees every node's work and two agents race the same subvolume.
    //
    // Which PATH is selectable is now the load-bearing part: placement is a fact the controllers
    // establish, so a parent's node lives in status, while a controller-written child's stays in
    // spec. `SnapshotRequest` has NO selector at all — it names no node, and every agent watches
    // every request, acting only when the named Volume is its own.
    for crd in all_crds() {
        let v = &crd.spec.versions[0];
        assert!(v.subresources.as_ref().is_some_and(|s| s.status.is_some()), "{}", crd.spec.names.kind);
        let want = match crd.spec.names.kind.as_str() {
            "OwnerBinding" | "Volume" => Some(".spec.nodeName"),
            "Workspace" | "Environment" => Some(".status.nodeName"),
            "SnapshotRequest" => None,
            other => panic!("unknown kind {other}"),
        };
        match want {
            None => assert!(
                v.selectable_fields.is_none(),
                "SnapshotRequest must have no selectableFields: it copies no node into spec"
            ),
            Some(path) => {
                let sel = v.selectable_fields.as_ref().expect("selectableFields");
                assert!(sel.iter().any(|f| f.json_path == path), "{} must select on {path}", crd.spec.names.kind);
                // Arrays cannot be selectable fields; `compatibleNodes` must never sneak in as one.
                assert!(!sel.iter().any(|f| f.json_path.contains("compatibleNodes")), "{}", crd.spec.names.kind);
            }
        }
    }
}

/// The five kinds, so a kind added to the group without a CRD entry cannot ship: `all_crds` is what
/// generates the manifest AND what the agent's startup precondition check reads.
#[test]
fn all_five_kinds_are_generated() {
    let kinds: Vec<String> = all_crds().into_iter().map(|c| c.spec.names.kind).collect();
    for k in ["Volume", "Workspace", "Environment", "OwnerBinding", "SnapshotRequest"] {
        assert!(kinds.iter().any(|g| g == k), "{k} missing from all_crds(): {kinds:?}");
    }
}

/// `phase` must be a schema `enum`, not a free-form string.
///
/// A typo in a phase does not fail today: `api::phase` falls back to a default on an unknown
/// string, so the controller wrote `running`, `WsState` spells that state `Ready`, and a healthy
/// workspace showed "Creating" in the UI forever. Nothing failed and nothing logged. An `enum` in
/// the schema turns that class of bug into a 422 at the API server.
#[test]
fn every_phase_is_a_schema_enum() {
    for crd in all_crds() {
        // OwnerBinding has no phase and needs none: `NamespaceReady` is its whole state.
        if crd.spec.names.kind == "OwnerBinding" {
            continue;
        }
        let status = crd.spec.versions[0]
            .schema
            .as_ref()
            .unwrap()
            .open_api_v3_schema
            .as_ref()
            .unwrap()
            .properties
            .as_ref()
            .unwrap()["status"]
            .clone();
        let phase = &status.properties.as_ref().unwrap()["phase"];
        assert!(
            phase.enum_.as_ref().is_some_and(|e| !e.is_empty()),
            "{}'s status.phase is a free-form string, not an enum",
            crd.spec.names.kind
        );
    }
}

/// Release 1 is ADDITIVE. `storage` arrives; the two legacy spec fields stay, optional, because a
/// cluster-wide prune before a per-node agent roll destroys the pointer an unmigrated object needs.
/// Task 11 is what removes them.
#[test]
fn release_one_adds_storage_and_keeps_the_legacy_spec_fields() {
    use kube::CustomResourceExt;
    use kloudlite_git_workspaces::crd::{Environment, Workspace};
    for crd in [Workspace::crd(), Environment::crd()] {
        let v = &crd.spec.versions[0];
        let root = v.schema.as_ref().unwrap().open_api_v3_schema.as_ref().unwrap();
        let spec = &root.properties.as_ref().unwrap()["spec"];
        let props = spec.properties.as_ref().unwrap();
        assert!(props.contains_key("storage"), "{} spec needs storage", crd.spec.names.kind);
        assert!(props.contains_key("nodeName"), "{}: do not prune nodeName in release 1", crd.spec.names.kind);
        assert!(props.contains_key("volumeRef"), "{}: do not prune volumeRef in release 1", crd.spec.names.kind);
        // Optional, though: the API stops writing them this release, so a required field would
        // reject every new object.
        let required = spec.required.clone().unwrap_or_default();
        assert!(!required.contains(&"nodeName".to_string()), "{}", crd.spec.names.kind);
        assert!(!required.contains(&"volumeRef".to_string()), "{}", crd.spec.names.kind);
        // The credential Secret path is deleted outright, not deprecated: nobody ever wrote that
        // Secret and no object carries it, so there is nothing to lose by pruning it.
        let schema = serde_json::to_string(&v.schema).unwrap();
        assert!(!schema.contains("credential_secret"), "{} still names credential_secret", crd.spec.names.kind);
    }
}

/// `lastPush` is gone and NOTHING replaces it on the Volume. "The latest snapshot" is a query over
/// SnapshotRequests by volume label — two controllers force-applying one status object under one
/// field manager prune each other's fields, which is what a second writer here would be.
#[test]
fn the_volume_status_has_no_push_pointer() {
    use kube::CustomResourceExt;
    let schema = serde_json::to_string(&kloudlite_git_workspaces::crd::Volume::crd().spec.versions[0].schema).unwrap();
    assert!(!schema.contains("lastPush"), "lastPush must be dropped");
    assert!(!schema.contains("lastSnapshot"), "and not replaced by a second writer's field");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p kloudlite-git-workspaces --test crd_yaml`
Expected: FAIL to compile — `crd::SnapshotRequest` does not exist.

- [ ] **Step 3: Write the implementation**

In `crates/workspaces/src/crd.rs`, replace `VolumeSource::GitRepo` (lines 55-67) with:

```rust
    /// A git repository on this platform, cloned at `branch` into the fresh subvolume by the
    /// workspace pod's INIT CONTAINER, not by the agent.
    ///
    /// No credential here and none in a Secret either: the clone runs inside the workspace, over
    /// SSH, as the owner, with the platform key already mounted at `k8s::USER_KEY_PATH`. The old
    /// `credential_secret` named a Secret nobody ever wrote and the agent had no permission to
    /// read — the git-seeding path was dead code that looked wired.
    GitRepo { repo: String, branch: String },
```

Delete `LastPush` (lines 70-77) and delete `last_push` from `VolumeStatus` (lines 123-124) with
**nothing in its place** — the comment that replaces it says why:

```rust
    // No `lastSnapshot` and no `lastPush`: "the newest snapshot of this volume" is a query over
    // `SnapshotRequest`s by the `kloudlite-git.io/volume` label. A second controller writing this
    // status object would prune the first one's fields — `patch_status` applies FORCED under one
    // `AGENT_FIELD_MANAGER`, and server-side apply removes fields a manager previously owned and no
    // longer sets, so the Volume reconciler's very next pass would delete whatever the snapshot
    // reconciler had just written.
```

Add, above `WorkspaceSpec`:

```rust
/// What the user asked of a parent object's storage. This is what the API used to author directly
/// as a `VolumeSpec`; the parent's reconciler is what turns it into a `Volume` now.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceStorage {
    pub quota_gb: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<VolumeSource>,
}

/// Every lifecycle state any of the five kinds reports, as ONE enum.
///
/// An enum rather than a `String` so schemars emits `enum` and the API server rejects a typo with a
/// 422. A free-form string is how `running` reached a `WsState` that spells that state `Ready`: the
/// projection's `serde_json::from_value` fell back to its default, so a healthy workspace showed
/// "Creating" in the UI indefinitely, with nothing failing and nothing logged.
///
/// One enum for five kinds rather than five, because the alternative is five near-identical types
/// and a `phase` field whose type a reader has to look up per kind. Which variants are legal for
/// which kind is the reconciler's business; the schema's job is to refuse a word nobody defined.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    /// Created, not yet claimed by a node.
    #[default]
    Pending,
    Creating,
    /// A workspace whose pod is Ready, or a Volume whose subvolume is materialized.
    Ready,
    /// An environment whose services are up. (`WsState` has no `Running`; `EnvState` has no
    /// `Ready` — the two projections disagree, and this enum is the union.)
    Running,
    Stopped,
    /// A btrfs operation is in flight.
    Working,
    /// A `SnapshotRequest` whose record is in the registry. Never re-run past this.
    Done,
    Error,
}
```

Then change `phase: String` to `phase: Phase` in `VolumeStatus`, `WorkspaceStatus`,
`EnvironmentStatus` and `SnapshotRequestStatus`, and give `OwnerBindingStatus` no phase at all (it
has none today and needs none — `NamespaceReady` is its whole state).

`api::phase` (`api.rs:328-330`) keeps taking `Option<&str>`; call it with
`st.map(|s| s.phase)` serialized through `serde_json::to_value(..).as_str()`, or more simply add:

```rust
impl Phase {
    /// The wire word, so a projection can go on matching on `&str` and the `/v1` docs' own enums
    /// (`model::WsState`, `model::EnvState`) stay the separate vocabulary they are.
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Pending => "pending",
            Phase::Creating => "creating",
            Phase::Ready => "ready",
            Phase::Running => "running",
            Phase::Stopped => "stopped",
            Phase::Working => "working",
            Phase::Done => "done",
            Phase::Error => "error",
        }
    }
}
```

Replace the `Workspace` kube attributes and spec/status (lines 164-209) with:

```rust
#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "kloudlite-git.io",
    version = "v1alpha1",
    kind = "Workspace",
    plural = "workspaces",
    shortname = "ws",
    status = "WorkspaceStatus",
    // Placement is a FACT the controllers establish, so it lives in status — and a status path is
    // a legal selectable field (only metadata is forbidden, and arrays are not allowed). An empty
    // value is what the unplaced watch selects on.
    selectable = ".status.nodeName",
    printcolumn = r#"{"name":"Owner","type":"string","jsonPath":".spec.owner"}"#,
    printcolumn = r#"{"name":"Node","type":"string","jsonPath":".status.nodeName"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#,
    derive = "PartialEq"
)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSpec {
    pub owner: String,
    /// The team this workspace is made in, or empty for the owner's personal namespace. A
    /// workspace's Kubernetes namespace is one per (team, owner) pair — see `ws_namespace` — so
    /// the same person's work in two teams never shares a namespace, a NetworkPolicy or a Secret.
    #[serde(default)]
    pub team: String,
    pub name: String,
    pub region: String,
    pub image: String,
    pub storage: WorkspaceStorage,
    pub desired_state: DesiredState,
    #[serde(default)]
    pub resources: PodResources,
    /// DEPRECATED, release 1 only. The API stopped writing these the moment placement moved into
    /// status, but they stay in the SCHEMA for one release: a CRD apply is cluster-wide and pruning
    /// is irreversible, while the agents roll per node — dropping them here would destroy the only
    /// pointer to the Volume of every object whose migration had not run yet. The startup migration
    /// reads them; Task 11 removes them once nothing carries them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_ref: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceStatus {
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// Where this object runs NOW. Empty means unplaced, which is exactly what the placement
    /// watch's `status.nodeName=` field selector matches.
    #[serde(default)]
    pub node_name: String,
    /// Every node that holds this object's volume — the memory placement uses when `nodeName` is
    /// empty. Nothing in this design writes more than one entry; nothing in it may assume there is
    /// only one (replication across nodes is a later design).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compatible_nodes: Vec<String>,
    /// The child `Volume`, reported rather than wished for: the reconciler creates it and then
    /// says so here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}
```

Make the same edits to `Environment` (lines 220-259): `selectable = ".status.nodeName"`, the `Node`
printcolumn repointed to `.status.nodeName`, add `pub storage: WorkspaceStorage,`, turn
`EnvironmentSpec`'s `volume_ref: String` / `node_name: String` into the same two deprecated
`Option<String>` fields with the same doc comment, and give `EnvironmentStatus` `phase: Phase` plus
the same `node_name`, `compatible_nodes` and `volume_ref` fields with the same doc comments.

Add `SNAPSHOT_FINALIZER` beside `SUBVOLUME_FINALIZER` (line 37):

```rust
/// Held while a `SnapshotRequest` may have work in flight.
///
/// Same reason `Volume` has one, and the reason the earlier "a plain delete, no finalizer" was
/// wrong: that is true of a FINISHED request and false of a working one. A delete during
/// `phase: working` leaves a btrfs RO snapshot, a stage file, an in-flight blob upload and a
/// possible `POST /commits` with no object left to record the outcome in — and the Volume's own
/// finalizer does not cover it, because a SnapshotRequest is deliberately not the Volume's child.
pub const SNAPSHOT_FINALIZER: &str = "kloudlite-git.io/snapshot";
```

Add the new kind, after `OwnerBindingStatus`:

```rust
/// One push, as an object: the request the user made and, in status, what it produced.
///
/// A CR rather than the annotation it replaces, because a push is a wish WITH AN OUTCOME and an
/// annotation has nowhere to put the outcome — the old design smuggled it into
/// `Volume.status.lastPush.at` by echoing the request's timestamp back.
///
/// Deliberately NOT owned by the Volume: a snapshot outlives a deleted workspace, because the
/// record it names still exists on the server tier. Deleting this object deletes no data.
/// ponytail: no snapshot deletion or retention yet; the GC sweep for blobs is unchanged.
#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "kloudlite-git.io",
    version = "v1alpha1",
    kind = "SnapshotRequest",
    plural = "snapshotrequests",
    shortname = "snap",
    status = "SnapshotRequestStatus",
    // NO `selectable`, deliberately. A node is a controller-owned fact and the API does not copy
    // facts into spec: the node this runs on is the named Volume's `spec.nodeName`, which moves
    // under node retirement and would go stale the instant it was copied here. Every agent watches
    // every request and acts only on the ones whose Volume is its own.
    // ponytail: every agent sees every request — two nodes today, so the fan-out is two. A
    // `spec.volume`-indexed reflector is the upgrade if the request count ever makes this hot.
    printcolumn = r#"{"name":"Volume","type":"string","jsonPath":".spec.volume"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Snapshot","type":"string","jsonPath":".status.snapshotId"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#,
    derive = "PartialEq"
)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotRequestSpec {
    /// The `Volume` to snapshot, by name. The whole spec: everything else about a push is either a
    /// fact a controller owns (the node) or an outcome (the record id).
    pub volume: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotRequestStatus {
    /// `pending` | `working` | `done` | `error`. A request is never re-run past `done`.
    pub phase: Phase,
    /// Mostly a "seen it" marker — the spec is immutable in practice, and `phase != done` is the
    /// real idempotency guard. Present because every status in this group carries one, and a
    /// reader who has to check per kind will eventually check wrong.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// The registry commit record's id — the snapshot itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lineage_tip: Option<String>,
    /// RFC 3339, when the record landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

/// The label a `SnapshotRequest` carries so `/v1/volumes/{id}/history` is one indexed list call
/// rather than a scan. Same rule as every other label here: a VIEW of `spec.volume`, never
/// authorization.
pub const VOLUME_LABEL: &str = "kloudlite-git.io/volume";
```

Extend `all_crds` (line 348) with `SnapshotRequest::crd(),`, and give `OwnerBinding` a
`selectable = ".spec.nodeName"` attribute plus `observed_generation` on its status (it already has
the field; keep it — the earlier design sketch dropped it). The doc comment at lines 263-264 saying
"not watched per node" is stale and must be replaced with "Watched by the agent on `spec.nodeName`:
this object is what makes an owner's per-team namespaces exist on that node".

- [ ] **Step 4: Regenerate the manifest and run the tests**

Run: `CRD_REGEN=1 cargo test -p kloudlite-git-workspaces --test crd_yaml`
Then: `cargo test -p kloudlite-git-workspaces --test crd_yaml`
Expected: PASS, and `git diff --stat deploy/k3s/crds.yaml` shows the file moved.

The rest of the workspace will not compile yet (`api.rs` and `controller.rs` still read
`spec.node_name`). That is expected and is Tasks 2-8; do not fix them here.

- [ ] **Step 5: Verify the load-bearing field-selector assumption on the real cluster**

The entire placement design rests on one API-server behaviour: an object created with NO status at
all must still match `--field-selector status.nodeName=`. The API server documents that "if
jsonPath refers to an absent field in a resource, the jsonPath evaluates to an empty string", but
this is worth one command before any Rust is written against it.

On the k3s VM (not this Mac), after applying the regenerated CRDs:

```sh
kubectl apply -f deploy/k3s/crds.yaml
kubectl create -f - <<'YAML'
apiVersion: kloudlite-git.io/v1alpha1
kind: Workspace
metadata: {name: selector-probe}
spec: {owner: probe, team: "", name: probe, region: r1, image: nginx:alpine,
       storage: {quotaGb: 1}, desiredState: stopped}
YAML
kubectl get workspaces --field-selector status.nodeName= -o name   # must list selector-probe
kubectl delete workspace selector-probe
```

If it lists nothing, STOP and report: placement would never trigger on create and only the requeue
backstop would save it, which changes Task 2's design (the fallback is a `status.nodeName` the API
seeds via a status write immediately after create). Record the observed output in the task report
either way.

- [ ] **Step 6: Commit**

```bash
git add crates/workspaces/src/crd.rs crates/workspaces/tests/crd_yaml.rs deploy/k3s/crds.yaml
git commit -m "Move placement into status and make a push an object"
```

---

## Task 2: Placement moves into the agent

**Files:**
- Create: `bins/agent/src/placement.rs` (moved verbatim from `crates/workspaces/src/placement.rs`)
- Create: `bins/agent/src/claim.rs`
- Delete: `crates/workspaces/src/placement.rs`; remove `pub mod placement;` from
  `crates/workspaces/src/lib.rs:1`
- Modify: `bins/agent/src/lib.rs:10` (module list), `bins/agent/src/controller.rs:138-176` (`run`)
- Test: `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Consumes: `crd::{Workspace, WorkspaceStatus, Environment, EnvironmentStatus, OwnerBinding, OwnerBindingSpec, binding_name}` from Task 1.
- Produces:
  - `pub async fn placement::place(client, region, owner, role) -> Result<Option<String>, kube::Error>` (unchanged signature)
  - `pub async fn claim::claim_workspace(w: &crd::Workspace, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr>`
  - `pub async fn claim::claim_environment(e: &crd::Environment, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr>`
  - `pub async fn claim::ensure_binding(ctx: &Arc<Ctx>, region: &str, owner: &str) -> Result<(), ReconcileErr>`
  - `Ctx::roles: Vec<String>` (from Node labels, D6) and `Ctx::region: String`.
  - `pub enum controller::Outcome { Permanent(String, &'static str), Transient(ReconcileErr) }` and
    `pub async fn controller::replace_status<K>(api, name, kind, resource_version, status) -> Result<(), ReconcileErr>`
    — the shared error classifier and the optimistic status write, both used by every later task.

- [ ] **Step 1: Write the failing test**

Add to `bins/agent/tests/reconcile.rs`:

```rust
const WS_STATUS: &str = "/apis/kloudlite-git.io/v1alpha1/workspaces/ws-1/status";
const BINDINGS: &str = "/apis/kloudlite-git.io/v1alpha1/ownerbindings";

fn ws_json(status: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kloudlite-git.io/v1alpha1",
        "kind": "Workspace",
        // `resourceVersion` is not decoration here: the claim carries it, and a test that omits it
        // would pass against a forced apply — the exact primitive this design refuses.
        "metadata": {"name": "ws-1", "uid": "ws-uid-1", "generation": 1, "resourceVersion": "42",
                     "labels": {"kloudlite-git.io/owner": "alice", "kloudlite-git.io/kind": "workspace",
                                "kloudlite-git.io/team": ""}},
        "spec": {"owner": "alice", "team": "", "name": "web", "region": "r1",
                 "image": "nginx:alpine", "storage": {"quotaGb": 20}, "desiredState": "running"},
        "status": status,
    })
}

fn workspace(status: serde_json::Value) -> crd::Workspace {
    serde_json::from_value(ws_json(status)).unwrap()
}

/// The claim is ONE status write, and it is a status write — an API-authored spec is never touched
/// by a controller. Everything downstream (the Volume's node, the PV's affinity, therefore the
/// pod's node) is derived from this one field.
///
/// It is a PUT (`replace_status`), not a forced apply: this is the one write in the system that
/// must be able to lose, and it carries the object's `resourceVersion` so that losing is a 409.
#[tokio::test]
async fn an_unplaced_workspace_is_claimed_with_one_optimistic_status_write() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PUT", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
            kloudlite_git_workspaces::kube_test::post(
                BINDINGS,
                serde_json::json!({"apiVersion": "kloudlite-git.io/v1alpha1", "kind": "OwnerBinding",
                                   "metadata": {"name": "r1-alice"},
                                   "spec": {"owner": "alice", "region": "r1", "nodeName": "node-a"}}),
            ),
        ],
    );

    kloudlite_git_agent::claim::claim_workspace(&workspace(serde_json::json!({})), &ctx).await.unwrap();

    let sent = rec.sent("PUT", WS_STATUS);
    assert_eq!(sent.len(), 1, "exactly one status write");
    assert_eq!(sent[0]["status"]["nodeName"], "node-a");
    assert_eq!(sent[0]["status"]["compatibleNodes"], serde_json::json!(["node-a"]));
    assert_eq!(
        sent[0]["metadata"]["resourceVersion"], "42",
        "without the resourceVersion the write cannot conflict, and the claim cannot race: {}", sent[0]
    );
    assert!(
        sent[0]["status"]["conditions"].as_array().unwrap().iter().any(|c| c["type"] == "Placed"),
        "the claim records itself as a condition: {}", sent[0]
    );
    assert!(rec.calls().iter().any(|c| c == &format!("POST {BINDINGS}")), "the binding must exist after a claim");
    assert!(
        !rec.calls().iter().any(|c| c == "PATCH /apis/kloudlite-git.io/v1alpha1/workspaces/ws-1"),
        "a controller never patches an API-authored spec: {:?}", rec.calls()
    );
}

/// `compatibleNodes` is the memory: a node not listed must leave the object alone so a listed one
/// can take it. Nothing today writes more than one entry, and nothing may assume that.
#[tokio::test]
async fn a_node_outside_compatible_nodes_does_not_claim() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![]);
    let w = workspace(serde_json::json!({"phase": "pending", "nodeName": "", "compatibleNodes": ["node-b"]}));

    kloudlite_git_agent::claim::claim_workspace(&w, &ctx).await.unwrap();
    assert!(rec.calls().is_empty(), "a node that does not hold the disk writes nothing: {:?}", rec.calls());
}

/// An already-placed object is not re-claimed; a stop keeps `status.nodeName` precisely so a later
/// start reconciles on the same node with no placement step.
#[tokio::test]
async fn an_already_placed_workspace_is_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![]);
    let w = workspace(serde_json::json!({"phase": "ready", "nodeName": "node-a", "compatibleNodes": ["node-a"]}));

    kloudlite_git_agent::claim::claim_workspace(&w, &ctx).await.unwrap();
    assert!(rec.calls().is_empty(), "{:?}", rec.calls());
}

/// Losing the race must be a REAL conflict. `Patch::Apply(..).force()` never conflicts — it is the
/// wrong primitive for the one write in this system that must race — so the claim is an optimistic
/// write carrying the object's `resourceVersion`, and a 409 means another node won.
///
/// The loser must also not create the OwnerBinding: that would bind an owner to a node that did
/// not win, and every later workspace of theirs would follow it.
#[tokio::test]
async fn a_claim_that_loses_the_race_re_reads_and_binds_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let conflict = Route {
        method: "PUT",
        path: WS_STATUS.into(),
        status: 409,
        body: serde_json::json!({
            "kind": "Status", "apiVersion": "v1", "status": "Failure",
            "reason": "Conflict", "code": 409,
            "message": "the object has been modified; please apply your changes to the latest version"
        }),
    };
    let won_by_peer = ws_json(serde_json::json!({"phase": "pending", "nodeName": "node-b", "compatibleNodes": ["node-b"]}));
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![conflict, kloudlite_git_workspaces::kube_test::get("/apis/kloudlite-git.io/v1alpha1/workspaces/ws-1", won_by_peer)],
    );

    let action = kloudlite_git_agent::claim::claim_workspace(&workspace(serde_json::json!({})), &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "the winner's write is our wake-up");
    assert!(
        !rec.calls().iter().any(|c| c.starts_with("POST")),
        "the loser must not bind the owner to a node it did not win: {:?}", rec.calls()
    );
    assert_eq!(rec.sent("PUT", WS_STATUS).len(), 1, "one attempt, then yield — not a retry loop");
}

/// `compatibleNodes` is a SET. Appending on a re-run grows the array without bound, and a
/// level-triggered reconciler re-runs by design.
#[tokio::test]
async fn claiming_twice_does_not_grow_compatible_nodes() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            Route { method: "PUT", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
            kloudlite_git_workspaces::kube_test::post(
                BINDINGS,
                serde_json::json!({"apiVersion": "kloudlite-git.io/v1alpha1", "kind": "OwnerBinding",
                                   "metadata": {"name": "r1-alice"},
                                   "spec": {"owner": "alice", "region": "r1", "nodeName": "node-a"}}),
            ),
        ],
    );
    // Already lists this node, but has no `nodeName` — the shape a claim that wrote
    // `compatibleNodes` and then lost its status write leaves behind.
    let w = workspace(serde_json::json!({"phase": "pending", "nodeName": "", "compatibleNodes": ["node-a"]}));

    kloudlite_git_agent::claim::claim_workspace(&w, &ctx).await.unwrap();
    let sent = rec.sent("PUT", WS_STATUS);
    assert_eq!(sent[0]["status"]["compatibleNodes"], serde_json::json!(["node-a"]), "union, not append");
}

/// A `cloneOf` needs the SOURCE's disk, so the new object's own (empty) `compatibleNodes` cannot
/// decide — the source's can. A node that does not hold the source must not claim, or the clone
/// stops being a local btrfs snapshot and becomes a network copy of data that is already here.
#[tokio::test]
async fn a_clone_is_claimed_only_where_its_source_lives() {
    let tmp = tempfile::tempdir().unwrap();
    let src = serde_json::json!({
        "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": "ws-src"},
        "spec": {"owner": "alice", "team": "", "name": "src", "region": "r1",
                 "image": "nginx:alpine", "storage": {"quotaGb": 20}, "desiredState": "running"},
        "status": {"phase": "ready", "nodeName": "node-b", "compatibleNodes": ["node-b"]}
    });
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![kloudlite_git_workspaces::kube_test::get("/apis/kloudlite-git.io/v1alpha1/workspaces/ws-src", src)],
    );
    let mut w = workspace(serde_json::json!({}));
    w.spec.storage.source = Some(crd::VolumeSource::CloneOf { volume: "ws-src".into() });

    kloudlite_git_agent::claim::claim_workspace(&w, &ctx).await.unwrap();
    assert!(
        !rec.calls().iter().any(|c| c.starts_with("PATCH")),
        "node-a does not hold ws-src's disk and must not claim its clone: {:?}", rec.calls()
    );
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p kloudlite-git-agent --test reconcile`
Expected: FAIL to compile — `kloudlite_git_agent::claim` does not exist.

- [ ] **Step 3: Move `placement.rs`, then write `claim.rs`**

```bash
git mv crates/workspaces/src/placement.rs bins/agent/src/placement.rs
```

In `bins/agent/src/placement.rs`, change the import at line 21 from `use crate::crd::{…}` to
`use kloudlite_git_workspaces::crd::{binding_name, OwnerBinding, OwnerBindingSpec};`, and in its
`#[cfg(test)] mod tests` change line 164 to
`use kloudlite_git_workspaces::kube_test::{conflict, get, mock_client, not_found, post, Route};`.
Nothing else in the file changes — same algorithm, new caller. Add `kloudlite-git-workspaces` with
`features = ["testkit"]` to `bins/agent/Cargo.toml`'s `[dev-dependencies]` if it is not already
there (the existing `tests/reconcile.rs` already uses `kube_test`, so it is).

Delete `pub mod placement;` from `crates/workspaces/src/lib.rs:1`, and add to
`bins/agent/src/lib.rs` next to line 10:

```rust
pub mod binding;
pub mod claim;
pub mod controller;
pub mod migrate;
pub mod placement;
pub mod snapshot;
```

Add to `Ctx` (`bins/agent/src/controller.rs:51-69`) and its constructor:

```rust
    /// This node's region, from `WS_REGION` — the other half of an `OwnerBinding`'s identity.
    pub region: String,
    /// The roles this node carries, read ONCE from its own `Node` labels at startup
    /// (`kloudlite-git.io/session`, `kloudlite-git.io/env`). A second, hand-maintained copy of a label
    /// the scheduler already reads is a second thing that can be wrong — see `k8s::placement`.
    pub roles: Vec<String>,
```

with `Ctx::new(client, engine, node, pool, region, roles)` taking them as arguments, and a helper in
`bins/agent/src/lib.rs::run` that fills `roles`:

```rust
/// The roles this node advertises. An unreadable Node object yields no roles, so the agent
/// converges what it already owns and claims nothing new — the safe direction, since the
/// alternative is claiming work for a pool this box may not have.
async fn node_roles(client: &kube::Client, node: &str) -> Vec<String> {
    let api: kube::Api<k8s_openapi::api::core::v1::Node> = kube::Api::all(client.clone());
    let Ok(Some(n)) = api.get_opt(node).await else {
        tracing::warn!(%node, "could not read this node's labels: claiming no unplaced work");
        return vec![];
    };
    let labels = n.metadata.labels.unwrap_or_default();
    ["session", "env"]
        .into_iter()
        .filter(|r| labels.get(&format!("kloudlite-git.io/{r}")).map(String::as_str) == Some("true"))
        .map(str::to_string)
        .collect()
}
```

Create `bins/agent/src/claim.rs`:

```rust
//! Placement, as a reconciler.
//!
//! An object with an empty `status.nodeName` is UNPLACED. Each agent runs a second watch selecting
//! exactly those, and the first node whose claim lands wins. The claim is a status write and only a
//! status write: the API authored this object's spec, and a controller that edits a user's desired
//! state is the failure this whole design exists to remove.
//!
//! Two nodes for now — one session, one env — so the claim checks no free space at all.
//! ponytail: no capacity check in the claim; `placement::pick` (the same algorithm, still here) is
//! what a second node of a role would consult, so growing the pool is a deploy, not a rewrite.

use crate::controller::{owner_ref_of_kind, patch_status, Ctx, ReconcileErr, RETRY};
use kube::api::{Api, PostParams};
use kube::runtime::controller::Action;
use kube::ResourceExt;
use kloudlite_git_workspaces::crd::{self, binding_name, OwnerBinding, OwnerBindingSpec};
use std::sync::Arc;

/// Whether THIS node may claim `object`, given the nodes already known to hold its data.
///
/// Empty `compatible` with no source means "nowhere holds it yet", which every node may claim. A
/// `cloneOf` is the exception the spec calls out: the new object holds nothing, but a local clone
/// needs the SOURCE's disk, so the source's memory decides.
fn may_claim(me: &str, compatible: &[String], source_compatible: Option<&[String]>) -> bool {
    if let Some(src) = source_compatible {
        return src.iter().any(|n| n == me);
    }
    compatible.is_empty() || compatible.iter().any(|n| n == me)
}

/// The `compatibleNodes` of a `cloneOf` source, when there is one. A source that has vanished
/// yields `Some([])` — nobody claims, and the object stays visible as unplaced rather than being
/// silently started somewhere with no data.
async fn source_nodes(
    ctx: &Arc<Ctx>,
    source: Option<&crd::VolumeSource>,
) -> Result<Option<Vec<String>>, ReconcileErr> {
    let Some(crd::VolumeSource::CloneOf { volume }) = source else { return Ok(None) };
    let api: Api<crd::Workspace> = Api::all(ctx.client.clone());
    let nodes = match api.get_opt(volume).await? {
        Some(w) => w.status.map(|s| s.compatible_nodes).unwrap_or_default(),
        None => vec![],
    };
    Ok(Some(nodes))
}

/// `union(existing, {me})` — a SET, computed and set, never appended.
///
/// A level-triggered reconciler re-runs by design, and an append grows the array every time. The
/// desired value is "every node known to hold this object's data, including me"; that is what gets
/// written, so re-running is a no-op instead of a leak.
fn with_me(existing: &[String], me: &str) -> Vec<String> {
    let mut out = existing.to_vec();
    if !out.iter().any(|n| n == me) {
        out.push(me.to_string());
    }
    out
}

pub async fn claim_workspace(w: &crd::Workspace, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    let st = w.status.clone().unwrap_or_default();
    if !st.node_name.is_empty() {
        // Already placed: the disk has not moved, so a later start reconciles here with no
        // placement step at all.
        return Ok(Action::await_change());
    }
    let src = source_nodes(ctx, w.spec.storage.source.as_ref()).await?;
    if !may_claim(&ctx.node, &st.compatible_nodes, src.as_deref()) {
        return Ok(Action::await_change());
    }
    let gen = w.meta().generation.unwrap_or(0);
    let status = serde_json::json!({
        "phase": crd::Phase::Pending,
        "nodeName": ctx.node,
        "compatibleNodes": with_me(&st.compatible_nodes, &ctx.node),
        "conditions": [crd::condition("Placed", true, "Claimed", &format!("claimed by {}", ctx.node), gen)],
    });
    let api: Api<crd::Workspace> = Api::all(ctx.client.clone());
    // Optimistic, carrying `metadata.resourceVersion`. NOT `patch_status`, which applies FORCED and
    // therefore never conflicts — with a forced apply two agents both "win" and the second silently
    // overwrites the first, which is the whole failure this write exists to prevent. A 409 means a
    // peer claimed it; its write is a watch event that brings us back, so there is nothing to retry
    // and nothing to bind.
    match replace_status(&api, w, "Workspace", status).await {
        Ok(()) => {}
        Err(ReconcileErr(e)) if e.contains("409") || e.contains("Conflict") => {
            tracing::info!(workspace = %w.name_any(), "lost the placement race; a peer claimed it");
            return Ok(Action::await_change());
        }
        Err(e) => return Err(e),
    }
    // Only the WINNER binds. Binding an owner to a node that lost would send every later workspace
    // of theirs to the wrong pool.
    ensure_binding(ctx, &w.spec.region, &w.spec.owner).await?;
    Ok(Action::await_change())
}

pub async fn claim_environment(e: &crd::Environment, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    let st = e.status.clone().unwrap_or_default();
    if !st.node_name.is_empty() {
        return Ok(Action::await_change());
    }
    // Environments have no clone-of-a-running-source path through placement: `clone_env` copies a
    // volume by id and the copy is materialized by the Volume controller, which needs the same
    // disk — the same rule, expressed through the same helper.
    let src = source_nodes(ctx, e.spec.storage.source.as_ref()).await?;
    if !may_claim(&ctx.node, &st.compatible_nodes, src.as_deref()) {
        return Ok(Action::await_change());
    }
    let gen = e.meta().generation.unwrap_or(0);
    let status = serde_json::json!({
        "phase": crd::Phase::Creating,
        "nodeName": ctx.node,
        "compatibleNodes": with_me(&st.compatible_nodes, &ctx.node),
        "conditions": [crd::condition("Placed", true, "Claimed", &format!("claimed by {}", ctx.node), gen)],
    });
    let api: Api<crd::Environment> = Api::all(ctx.client.clone());
    match replace_status(&api, e, "Environment", status).await {
        Ok(()) => {}
        Err(ReconcileErr(msg)) if msg.contains("409") || msg.contains("Conflict") => {
            tracing::info!(environment = %e.name_any(), "lost the placement race; a peer claimed it");
            return Ok(Action::await_change());
        }
        Err(err) => return Err(err),
    }
    ensure_binding(ctx, &e.spec.region, &e.spec.owner).await?;
    Ok(Action::await_change())
}

/// The `{region, owner}` binding for this node, created atomically. A 409 means a peer got there
/// first and its answer is as good as ours — the binding is what makes the per-owner namespace
/// reconciler run, not a second placement decision.
pub async fn ensure_binding(ctx: &Arc<Ctx>, region: &str, owner: &str) -> Result<(), ReconcileErr> {
    let api: Api<OwnerBinding> = Api::all(ctx.client.clone());
    let name = binding_name(region, owner);
    let b = OwnerBinding::new(
        &name,
        OwnerBindingSpec { owner: owner.into(), region: region.into(), node_name: ctx.node.clone() },
    );
    match api.create(&PostParams::default(), &b).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(s)) if s.code == 409 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

```

Trim the `use` list to what the finished module actually references — the block above imports
`RETRY` and `owner_ref_of_kind` for symmetry with its siblings and does not use either; clippy
`-D warnings` will say so.

Add to `bins/agent/src/controller.rs`, beside `patch_status` (line 582), the two pieces of shared
plumbing every later task builds on:

```rust
/// An OPTIMISTIC status write: `replace_status` carrying the object's current
/// `metadata.resourceVersion`, so a concurrent writer makes this a 409.
///
/// The counterpart to `patch_status`, and the difference is the whole point. `patch_status` applies
/// FORCED, which is right for a write only one node can make (its own node's objects) and wrong for
/// the one write two nodes race: a forced apply has no precondition, never conflicts, and lets both
/// claimants believe they won. Use this for the claim; use `patch_status` for everything else.
pub async fn replace_status<K>(api: &Api<K>, obj: &K, kind: &str, status: serde_json::Value) -> Result<(), ReconcileErr>
where
    K: Resource + Clone + serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
    K::DynamicType: Default,
{
    let name = obj.name_any();
    let body = serde_json::json!({
        "apiVersion": format!("{}/{}", crd::GROUP, crd::VERSION),
        "kind": kind,
        "metadata": {
            "name": name,
            // The precondition. Without it the API server accepts the write unconditionally and
            // the race is silently lost by whoever wrote first.
            "resourceVersion": obj.meta().resource_version.clone().unwrap_or_default(),
        },
        "status": status,
    });
    let bytes = serde_json::to_vec(&body).map_err(|e| ReconcileErr(e.to_string()))?;
    api.replace_status(&name, &kube::api::PostParams::default(), bytes).await?;
    Ok(())
}

/// Why a reconcile could not finish, and therefore what to do about it.
///
/// Today every failure is `Action::requeue(RETRY)`, which makes a spec that can never work look
/// exactly like a registry that is briefly down — the same line in the log, forever, at one a
/// minute. The new `storage.source` inputs make that untenable: a `cloneOf` naming a workspace that
/// does not exist, a `restoreOf` whose snapshot no `done` request carries, a Volume pinned to
/// another node — none of these get better by being retried.
pub enum Outcome {
    /// Nothing will change this without a new spec. Write the condition, stop.
    Permanent(String, &'static str),
    /// The world is briefly unavailable. Return `Err` and take `error_policy`'s backoff.
    Transient(ReconcileErr),
}

impl From<kube::Error> for Outcome {
    /// An API-server error is transient by default — a 5xx, a timeout, a lost connection. A 404 on
    /// a REFERENCE (a `cloneOf` source, say) is permanent, but only the caller knows which
    /// reference it was reading, so that classification is made at the call site, not here.
    fn from(e: kube::Error) -> Self {
        Outcome::Transient(ReconcileErr(e.to_string()))
    }
}

/// Turn an `Outcome` into the reconcile's answer, writing the condition on the permanent path.
///
/// `await_change()` on permanent, deliberately: the object is wrong and the next thing that can
/// help is a human or a new spec, both of which arrive as watch events.
pub async fn settle<K, F>(outcome: Outcome, obj: &K, kind: &str, gen: i64, write: F, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr>
where
    K: Resource<DynamicType = ()> + ResourceExt,
    F: FnOnce(Condition) -> serde_json::Value,
{
    match outcome {
        Outcome::Permanent(msg, reason) => {
            tracing::warn!(kind = %kind, name = %obj.name_any(), reason = %reason, error = %msg, "permanent failure; not retrying");
            let cond = crd::condition("Ready", false, reason, &msg, gen);
            let api: Api<K> = Api::all(ctx.client.clone());
            patch_status(&api, &obj.name_any(), kind, write(cond)).await?;
            Ok(Action::await_change())
        }
        Outcome::Transient(e) => Err(e),
    }
}
```

`reason` is a CamelCase token, never a sentence — `meta/v1.Condition` requires it and
`kubectl wait --for=condition=…` matches on it. The `write` closure exists because each kind's
status has a different shape; every call site passes a one-line builder for its own status.

In `controller.rs::run` (lines 138-176), add the two unplaced controllers, gated on role:

```rust
    // Unplaced objects, one watch per ROLE this node carries. `status.nodeName=` (empty) is a
    // legal field selector because the CRD declares `.status.nodeName` selectable — and the claim
    // is what moves the object out of this watch and into the node's own, with no poll in between.
    let unplaced = watcher::Config::default().fields("status.nodeName=");
    let claim_ws = ctx.roles.iter().any(|r| r == "session").then(|| {
        Controller::new(Api::<crd::Workspace>::all(ctx.client.clone()), unplaced.clone())
            .shutdown_on_signal()
            .run(|w, c| async move { claim::claim_workspace(&w, &c).await }, error_policy, ctx.clone())
            .for_each(|r| async move {
                if let Err(e) = r {
                    tracing::warn!(error = %e, "workspace claim")
                }
            })
    });
    let claim_env = ctx.roles.iter().any(|r| r == "env").then(|| {
        Controller::new(Api::<crd::Environment>::all(ctx.client.clone()), unplaced)
            .shutdown_on_signal()
            .run(|e, c| async move { claim::claim_environment(&e, &c).await }, error_policy, ctx.clone())
            .for_each(|r| async move {
                if let Err(e) = r {
                    tracing::warn!(error = %e, "environment claim")
                }
            })
    });
```

and join them alongside the existing three with
`futures::future::OptionFuture::from(claim_ws)` / `…(claim_env)`.

Also make `patch_status`, `RETRY`, `TICK` and `owner_ref` `pub` in `controller.rs` (they are shared
by every new module), and add `pub fn owner_ref_of_kind<K: Resource<DynamicType = ()>>(obj: &K)`
as a rename of the existing private `owner_ref` (line 259).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p kloudlite-git-agent --test reconcile` and
`cargo test -p kloudlite-git-agent placement`
Expected: the four new claim tests PASS and every moved `placement` unit test still passes.

- [ ] **Step 5: Commit**

```bash
git add bins/agent/src/placement.rs bins/agent/src/claim.rs bins/agent/src/lib.rs \
        bins/agent/src/controller.rs bins/agent/tests/reconcile.rs crates/workspaces/src/lib.rs
git commit -m "Claim unplaced workspaces in the agent, not the API"
```

---

## Task 3: The OwnerBinding reconciler

**Files:**
- Create: `bins/agent/src/binding.rs`
- Modify: `bins/agent/src/controller.rs` (`run`: add the controller and its `Workspace` watch)
- Test: `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Consumes: `claim::ensure_binding` (Task 2), `Ctx { client, node, region, api_service_account, api_namespace }`.
- Produces: `pub async fn binding::apply_binding(b: &crd::OwnerBinding, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr>`;
  `pub const NAMESPACE_READY: &str = "NamespaceReady";`;
  `pub async fn binding::namespace_ready(ctx: &Arc<Ctx>, region: &str, owner: &str) -> Result<bool, ReconcileErr>`.

- [ ] **Step 1: Write the failing test**

```rust
const BINDING_STATUS: &str = "/apis/kloudlite-git.io/v1alpha1/ownerbindings/r1-alice/status";

fn binding_json() -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "OwnerBinding",
        "metadata": {"name": "r1-alice", "uid": "ob-uid-1", "generation": 1},
        "spec": {"owner": "alice", "region": "r1", "nodeName": "node-a"}
    })
}

/// The per-owner shared objects have exactly ONE owner now. They used to be re-ensured by the
/// workspace reconciler and the environment reconciler on every pass, which is two writers for one
/// object and a namespace deleted by whichever ran last.
#[tokio::test]
async fn a_binding_ensures_one_namespace_per_team_in_use_and_reports_ready() {
    let tmp = tempfile::tempdir().unwrap();
    let ws_list = serde_json::json!({
        "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "WorkspaceList", "metadata": {},
        "items": [ws_json(serde_json::json!({"nodeName": "node-a"}))]
    });
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_git_workspaces::kube_test::get("/apis/kloudlite-git.io/v1alpha1/workspaces", ws_list),
            Route { method: "PATCH", path: "/api/v1/namespaces/ws-alice".into(), status: 200,
                    body: serde_json::json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "ws-alice"}}) },
            Route { method: "PATCH", path: "/apis/networking.k8s.io/v1/namespaces/ws-alice/networkpolicies/default-deny".into(),
                    status: 200, body: serde_json::json!({"apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy", "metadata": {"name": "default-deny"}}) },
            Route { method: "PATCH", path: "/api/v1/namespaces/ws-alice/limitranges/workspace-defaults".into(), status: 200,
                    body: serde_json::json!({"apiVersion": "v1", "kind": "LimitRange", "metadata": {"name": "workspace-defaults"}}) },
            Route { method: "PATCH", path: "/apis/rbac.authorization.k8s.io/v1/namespaces/ws-alice/rolebindings/api-secrets".into(),
                    status: 200, body: serde_json::json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "RoleBinding", "metadata": {"name": "api-secrets"}}) },
            Route { method: "PATCH", path: BINDING_STATUS.into(), status: 200, body: binding_json() },
        ],
    );
    let b: crd::OwnerBinding = serde_json::from_value(binding_json()).unwrap();

    kloudlite_git_agent::binding::apply_binding(&b, &ctx).await.unwrap();

    assert!(rec.calls().iter().any(|c| c == "PATCH /api/v1/namespaces/ws-alice"), "{:?}", rec.calls());
    let sent = rec.sent("PATCH", "/api/v1/namespaces/ws-alice");
    assert!(
        sent[0]["metadata"].get("ownerReferences").is_none(),
        "a namespace shared by every workspace this user owns must never be GC'd with one binding: {}", sent[0]
    );
    let limit = rec.sent("PATCH", "/api/v1/namespaces/ws-alice/limitranges/workspace-defaults");
    assert!(limit[0]["metadata"].get("ownerReferences").is_none(), "a quota ceiling must not vanish with a binding rewrite");
    let st = rec.sent("PATCH", BINDING_STATUS);
    assert_eq!(st.len(), 1);
    assert!(
        st[0]["status"]["conditions"].as_array().unwrap().iter()
            .any(|c| c["type"] == "NamespaceReady" && c["status"] == "True"),
        "{}", st[0]
    );
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p kloudlite-git-agent --test reconcile a_binding_ensures`
Expected: FAIL to compile — `kloudlite_git_agent::binding` does not exist.

- [ ] **Step 3: Write `bins/agent/src/binding.rs`**

```rust
//! The per-owner shared objects, owned by exactly one reconciler.
//!
//! They used to be re-ensured by the workspace reconciler AND the environment reconciler on every
//! pass — two writers for one object, which is how a namespace ends up recreated by whichever ran
//! last. An `OwnerBinding` says "this owner's work lives on this node", so it is the natural owner
//! of "this owner has namespaces on this node".
//!
//! ponytail: bindings are never deleted; a node-retirement path re-homes them later.

use crate::controller::{ensure, patch_status, Ctx, ReconcileErr, TICK};
use k8s_openapi::api::core::v1::{LimitRange, Namespace};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::api::rbac::v1::RoleBinding;
use kube::api::{Api, ListParams};
use kube::runtime::controller::Action;
use kube::ResourceExt;
use kloudlite_git_workspaces::crd::{self, binding_name, ws_namespace};
use kloudlite_git_workspaces::k8s;
use std::collections::BTreeSet;
use std::sync::Arc;

pub const NAMESPACE_READY: &str = "NamespaceReady";

/// Every team this owner has a workspace in ON THIS NODE, plus the personal namespace.
///
/// The personal one is unconditional: a first workspace's reconcile waits on `NamespaceReady`, and
/// gating the namespace on a workspace that is itself waiting for the namespace is a deadlock.
async fn teams_in_use(ctx: &Arc<Ctx>, owner: &str) -> Result<BTreeSet<String>, ReconcileErr> {
    let api: Api<crd::Workspace> = Api::all(ctx.client.clone());
    let lp = ListParams::default().labels(&format!("{}={owner}", k8s::OWNER_LABEL));
    let mut teams = BTreeSet::from([String::new()]);
    for w in api.list(&lp).await?.items {
        if w.status.as_ref().map(|s| s.node_name.as_str()) == Some(ctx.node.as_str()) {
            teams.insert(w.spec.team.clone());
        }
    }
    Ok(teams)
}

pub async fn apply_binding(b: &crd::OwnerBinding, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    let owner = b.spec.owner.clone();
    let gen = b.meta().generation.unwrap_or(0);
    let owner_ref = b.controller_owner_ref(&()).ok_or_else(|| ReconcileErr("binding has no uid".into()))?;
    for team in teams_in_use(ctx, &owner).await? {
        let ns = ws_namespace(&owner, &team);
        // No ownerReference on the namespace or the LimitRange: the namespace is shared by every
        // workspace this user owns IN THIS TEAM, and an owner's quota ceiling must not vanish with
        // a binding rewrite. See `crd::ws_namespace`.
        ensure(&Api::<Namespace>::all(ctx.client.clone()), &k8s::namespace(&ns, &owner, "workspace", None)).await?;
        ensure(
            &Api::<LimitRange>::namespaced(ctx.client.clone(), &ns),
            &k8s::limit_range(&ns, &owner, "workspace", &crd::PodResources::default(), None),
        )
        .await?;
        let policies = Api::<NetworkPolicy>::namespaced(ctx.client.clone(), &ns);
        for p in k8s::default_policies(&ns, &owner, &owner_ref) {
            ensure(&policies, &p).await?;
        }
        // Scope the API's Secret access to THIS namespace. The alternative is a cluster-wide
        // `secrets: create` for the API, which would include the agent's own credentials.
        ensure(
            &Api::<RoleBinding>::namespaced(ctx.client.clone(), &ns),
            &k8s::api_secret_binding(&ns, &owner, &ctx.api_service_account, &ctx.api_namespace),
        )
        .await?;
    }
    let status = serde_json::json!({
        "observedGeneration": gen,
        "conditions": [crd::condition(NAMESPACE_READY, true, "Converged", "namespaces exist on this node", gen)],
    });
    let api: Api<crd::OwnerBinding> = Api::all(ctx.client.clone());
    patch_status(&api, &b.name_any(), "OwnerBinding", status).await?;
    Ok(Action::await_change())
}

/// Whether the owner's binding on this node reports `NamespaceReady`. A missing binding is "not
/// ready", never an error: it is the ordinary gap between a claim and the binding reconcile.
pub async fn namespace_ready(ctx: &Arc<Ctx>, region: &str, owner: &str) -> Result<bool, ReconcileErr> {
    let api: Api<crd::OwnerBinding> = Api::all(ctx.client.clone());
    let Some(b) = api.get_opt(&binding_name(region, owner)).await? else { return Ok(false) };
    Ok(b.status.is_some_and(|s| {
        s.conditions.iter().any(|c| c.type_ == NAMESPACE_READY && c.status == "True")
    }))
}

/// How long a waiter sleeps between `NamespaceReady` checks. Re-exported so the two parent
/// reconcilers cannot disagree about it.
pub const WAIT: std::time::Duration = TICK;
```

In `controller.rs::run`, add:

```rust
    let bindings = Controller::new(Api::<crd::OwnerBinding>::all(ctx.client.clone()), mine.clone())
        // A new Workspace of this owner may need a new TEAM namespace, so the binding reconciles
        // on it. Mapped by `spec.owner`, not by ownerReference: the binding is not the Workspace's
        // parent, it is the thing that makes its namespace exist.
        .watches(Api::<crd::Workspace>::all(ctx.client.clone()), watcher::Config::default(), {
            let region = ctx.region.clone();
            move |w: crd::Workspace| {
                Some(kube::runtime::reflector::ObjectRef::<crd::OwnerBinding>::new(&crd::binding_name(
                    &region,
                    &w.spec.owner,
                )))
            }
        })
        .shutdown_on_signal()
        .run(|b, c| async move { binding::apply_binding(&b, &c).await }, error_policy, ctx.clone())
        .for_each(|r| async move {
            if let Err(e) = r {
                tracing::warn!(error = %e, "ownerbinding reconcile")
            }
        });
```

and join it with the rest.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p kloudlite-git-agent --test reconcile a_binding_ensures`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add bins/agent/src/binding.rs bins/agent/src/controller.rs bins/agent/tests/reconcile.rs
git commit -m "Give per-owner namespaces one reconciler that owns them"
```

---

## Task 4: The Workspace reconciler creates and waits for its Volume

**Files:**
- Modify: `bins/agent/src/controller.rs:596-742` (`reconcile_workspace`, `volume_node`,
  `apply_workspace`, `write_ws_status`), and `run` (lines 152-162) for the new watches
- Modify: `crates/workspaces/src/k8s.rs:450-518` (`workspace_pod`) — add the init container
- Test: `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Consumes: `crd::WorkspaceStorage`, `crd::WorkspaceStatus` (Task 1); `binding::namespace_ready`,
  `binding::NAMESPACE_READY` (Task 3); `controller::{ensure, create_if_absent, delete_ignoring_404, patch_status, owner_ref_of_kind, TICK}`.
- Produces:
  - `pub async fn controller::apply_workspace(w: &crd::Workspace, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr>`
  - `pub async fn controller::ensure_child_volume<P>(parent: &P, owner, team, region, storage, node, kind, ctx) -> Result<crd::Volume, ReconcileErr>`
    where `P: Resource<DynamicType = ()>` — used again by Task 6.
  - `k8s::git_init_container(source: &crd::VolumeSource, init_image: &str, ssh_host: &str, ssh_port: &str) -> Option<Container>`
  - `k8s::workspace_pod(spec: &WorkspaceSpec, id: &str, ctx: &PodContext, init: Option<Container>) -> Pod`

- [ ] **Step 1: Write the failing test**

```rust
/// The stuck pod, as a test: a workspace whose disk does not exist yet must not get a pod. The
/// symptom this fixes was a pod wedged forever on `path … does not exist`, because the workspace
/// reconciler never looked at its volume's status.
#[tokio::test]
async fn a_workspace_with_an_unready_volume_creates_no_pod() {
    let tmp = tempfile::tempdir().unwrap();
    let vol = serde_json::json!({
        "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "ws-1", "uid": "vol-uid-1"},
        "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20},
        "status": {"phase": "working", "subvolumePresent": false}
    });
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_git_workspaces::kube_test::get("/apis/kloudlite-git.io/v1alpha1/volumes/ws-1", vol),
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        ],
    );
    let w = workspace(serde_json::json!({"nodeName": "node-a", "compatibleNodes": ["node-a"]}));

    let action = kloudlite_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)));
    assert!(
        !rec.calls().iter().any(|c| c.contains("/pods")),
        "no pod may exist before its disk does: {:?}", rec.calls()
    );
    let st = rec.sent("PATCH", WS_STATUS);
    assert_eq!(st.last().unwrap()["status"]["phase"], "creating");
    assert!(
        st.last().unwrap()["status"]["conditions"].as_array().unwrap().iter()
            .any(|c| c["type"] == "VolumeReady" && c["status"] == "False"),
        "{}", st.last().unwrap()
    );
}

/// The child is created by the parent, from the parent's placement, with an ownerReference — which
/// is what makes `DELETE workspace` reclaim the disk with no ordering logic in the API.
#[tokio::test]
async fn a_placed_workspace_creates_its_volume_child_on_its_own_node() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_git_workspaces::kube_test::not_found("/apis/kloudlite-git.io/v1alpha1/volumes/ws-1"),
            kloudlite_git_workspaces::kube_test::post(
                "/apis/kloudlite-git.io/v1alpha1/volumes",
                serde_json::json!({"apiVersion": "kloudlite-git.io/v1alpha1", "kind": "Volume",
                                   "metadata": {"name": "ws-1"},
                                   "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20}}),
            ),
            Route { method: "PATCH", path: WS_STATUS.into(), status: 200, body: ws_json(serde_json::json!({})) },
        ],
    );
    let w = workspace(serde_json::json!({"nodeName": "node-a", "compatibleNodes": ["node-a"]}));

    kloudlite_git_agent::controller::apply_workspace(&w, &ctx).await.unwrap();

    let sent = rec.sent("POST", "/apis/kloudlite-git.io/v1alpha1/volumes");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["spec"]["nodeName"], "node-a", "the Volume is created FROM status.nodeName");
    assert_eq!(sent[0]["spec"]["quotaGb"], 20);
    let refs = sent[0]["metadata"]["ownerReferences"].as_array().expect("an ownerReference");
    assert_eq!(refs[0]["kind"], "Workspace");
    assert_eq!(refs[0]["name"], "ws-1");
    assert_eq!(refs[0]["controller"], true);
}

/// Git seeding, end to end in one object: an init container that clones over SSH with the owner's
/// platform key, and no token Secret anywhere — the API named one nobody wrote and the agent could
/// not read.
#[test]
fn a_git_seeded_pod_carries_an_init_container_with_the_key_and_no_token() {
    use kloudlite_git_workspaces::{crd, k8s};
    let spec = crd::WorkspaceSpec {
        owner: "alice".into(),
        team: String::new(),
        name: "web".into(),
        region: "r1".into(),
        image: "nginx:alpine".into(),
        storage: crd::WorkspaceStorage {
            quota_gb: 20,
            source: Some(crd::VolumeSource::GitRepo { repo: "alice/site".into(), branch: "main".into() }),
        },
        desired_state: crd::DesiredState::Running,
        resources: Default::default(),
    };
    let init = k8s::git_init_container(spec.storage.source.as_ref().unwrap(), "alpine/git:2.45.2", "git.example.com", "22");
    let init = init.expect("a gitRepo source seeds with an init container");
    let pod = k8s::workspace_pod(&spec, "ws-1", &test_pod_ctx(), Some(init));

    let inits = pod.spec.as_ref().unwrap().init_containers.as_ref().expect("init containers");
    assert_eq!(inits.len(), 1);
    assert_eq!(inits[0].image.as_deref(), Some("alpine/git:2.45.2"), "pinned, so seeding works with any workspace image");
    let mounts: Vec<&str> = inits[0].volume_mounts.as_ref().unwrap().iter().map(|m| m.mount_path.as_str()).collect();
    assert!(mounts.contains(&"/workspace"));
    assert!(mounts.contains(&k8s::USER_KEY_PATH));
    let env: std::collections::HashMap<&str, String> = inits[0]
        .env
        .as_ref()
        .unwrap()
        .iter()
        .map(|e| (e.name.as_str(), e.value.clone().unwrap_or_default()))
        .collect();
    assert_eq!(env["URL"], "ssh://git@git.example.com:22/alice/site.git");
    assert_eq!(env["BRANCH"], "main");
    assert!(env["GIT_SSH_COMMAND"].contains(k8s::USER_KEY_PATH));
    let rendered = serde_json::to_string(&pod).unwrap();
    assert!(!rendered.contains("token"), "no credential Secret is involved any more: {rendered}");
    // Idempotent: a pod restart must never re-clone over a user's work.
    assert!(inits[0].command.as_ref().unwrap().join(" ").contains("ls -A /workspace"));
    // The key mount stops being optional for a seeded workspace — the clone cannot work without it.
    let vols = pod.spec.as_ref().unwrap().volumes.as_ref().unwrap();
    let key = vols.iter().find(|v| v.name == "user-key").unwrap();
    assert_eq!(key.secret.as_ref().unwrap().optional, Some(false));
}

fn test_pod_ctx() -> kloudlite_git_workspaces::k8s::PodContext<'static> {
    kloudlite_git_workspaces::k8s::PodContext {
        pool: "/pool",
        node_name: "node-a",
        owner_ref: k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
            api_version: "kloudlite-git.io/v1alpha1".into(),
            kind: "Workspace".into(),
            name: "ws-1".into(),
            uid: "ws-uid-1".into(),
            controller: Some(true),
            block_owner_deletion: Some(true),
        },
        runtime_class: None,
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p kloudlite-git-agent --test reconcile`
Expected: FAIL to compile — `k8s::git_init_container` does not exist and `workspace_pod` takes
three arguments.

- [ ] **Step 3: Write the implementation**

In `crates/workspaces/src/k8s.rs`, add above `workspace_pod`:

```rust
/// The container that seeds a `gitRepo` workspace, or `None` for any other source.
///
/// It runs INSIDE the workspace, over SSH, as the owner, with the platform key the pod already
/// mounts. That is the whole reason the credential Secret is gone: there is no third party to mint
/// a token for, and the git tier already decides what this key may read.
///
/// `repo` is `owner/name`, never a URL, and the host comes from the agent's env — a caller cannot
/// point this at an arbitrary endpoint, which would be an egress and SSRF primitive available to
/// anyone who can create a workspace.
///
/// `--depth 1 --single-branch`: a workspace wants the branch's tip to start from.
/// ponytail: shallow, so `git log` in the workspace shows one commit; deepen on demand if anyone
/// asks for the history they did not ask to clone.
pub fn git_init_container(
    source: &crate::crd::VolumeSource,
    init_image: &str,
    ssh_host: &str,
    ssh_port: &str,
) -> Option<Container> {
    let crate::crd::VolumeSource::GitRepo { repo, branch } = source else { return None };
    let url = if ssh_port.is_empty() || ssh_port == "22" {
        format!("ssh://git@{ssh_host}/{repo}.git")
    } else {
        format!("ssh://git@{ssh_host}:{ssh_port}/{repo}.git")
    };
    Some(Container {
        name: "git-seed".to_string(),
        image: Some(init_image.to_string()),
        // The empty-dir check is what makes this idempotent: a pod restart, a node reboot or a
        // second reconcile must never clone over work the user has done.
        command: Some(vec![
            "sh".to_string(),
            "-c".to_string(),
            "set -e; [ \"$(ls -A /workspace)\" ] || git clone --depth 1 --single-branch --branch \"$BRANCH\" \"$URL\" /workspace"
                .to_string(),
        ]),
        env: Some(vec![
            EnvVar { name: "URL".to_string(), value: Some(url), ..Default::default() },
            EnvVar { name: "BRANCH".to_string(), value: Some(branch.clone()), ..Default::default() },
            git_ssh_command(),
        ]),
        volume_mounts: Some(vec![
            VolumeMount { name: "live".to_string(), mount_path: "/workspace".to_string(), ..Default::default() },
            VolumeMount {
                name: "user-key".to_string(),
                mount_path: USER_KEY_PATH.to_string(),
                read_only: Some(true),
                ..Default::default()
            },
        ]),
        // Same user as the main container, so the files land owned by whoever will edit them.
        security_context: Some(hardened()),
        ..Default::default()
    })
}

/// The one definition of `GIT_SSH_COMMAND`, shared by the workspace container and the seeder. Two
/// copies of an ssh invocation that must agree is two invocations that will not.
fn git_ssh_command() -> EnvVar {
    EnvVar {
        name: "GIT_SSH_COMMAND".to_string(),
        value: Some(format!(
            "ssh -i {USER_KEY_PATH}/id_ed25519 -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new"
        )),
        ..Default::default()
    }
}
```

Change `workspace_pod`'s signature to
`pub fn workspace_pod(spec: &WorkspaceSpec, id: &str, ctx: &PodContext, init: Option<Container>) -> Pod`,
replace its inline `EnvVar` (lines 483-489) with `vec![git_ssh_command()]`, set
`pod_spec.init_containers = init.clone().map(|c| vec![c]);`, and make the key mount required when
seeding:

```rust
    // Required, not optional, for a seeded workspace: the init container cannot clone without the
    // key, and an optional mount would silently start a pod that clones nothing and reports Ready.
    volumes: Some(vec![claim_volume(id), user_key_volume(init.is_some())]),
```

with `fn user_key_volume(required: bool) -> Volume` taking `optional: Some(!required)` at line 235.

In `bins/agent/src/controller.rs`, replace `volume_node` and `apply_workspace` (lines 601-742) with:

```rust
/// Create this parent's `Volume` child if it is missing, and hand back what the API server holds.
///
/// The child takes the PARENT's name (`D1`): the id is already the registry key, the PV name, the
/// PVC name and the URL segment, and an ownerReference — not a name — is what makes it a child.
pub async fn ensure_child_volume<P>(
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
    let id = parent.name_any();
    let api: Api<crd::Volume> = Api::all(ctx.client.clone());
    if let Some(v) = api.get_opt(&id).await? {
        return Ok(v);
    }
    let mut vol = crd::Volume::new(
        &id,
        crd::VolumeSpec {
            owner: owner.to_string(),
            team: team.to_string(),
            // FROM `status.nodeName`, never recomputed. `volume_node`'s mismatch guard below is the
            // belt to this brace: audit H1 ("a Workspace never names a node its Volume does not")
            // holds by construction because the Volume is authored here.
            node_name: node.to_string(),
            region: region.to_string(),
            quota_gb: storage.quota_gb,
            source: storage.source.clone(),
        },
    );
    vol.metadata.owner_references = Some(vec![owner_ref_of_kind(parent)?]);
    vol.metadata.labels = Some(std::collections::BTreeMap::from([
        (k8s::OWNER_LABEL.to_string(), owner.to_string()),
        (k8s::KIND_LABEL.to_string(), kind.to_string()),
        (k8s::TEAM_LABEL.to_string(), team.to_string()),
    ]));
    match api.create(&kube::api::PostParams::default(), &vol).await {
        Ok(v) => Ok(v),
        // Lost a race with our own earlier pass. Read back what won.
        Err(kube::Error::Api(s)) if s.code == 409 => Ok(api.get(&id).await?),
        Err(e) => Err(e.into()),
    }
}

/// Whether the child's disk actually exists. A parent acts on a child only by reading the child's
/// status, never by guessing — and "the object exists" is not "the subvolume exists".
fn volume_is_ready(v: &crd::Volume) -> bool {
    v.status.as_ref().is_some_and(|s| s.phase == crd::Phase::Ready && s.subvolume_present)
}

/// The source references that can be wrong forever, checked ONCE before a Volume is created.
///
/// These are the new permanent-error inputs `storage.source` brings, and they are exactly the ones
/// that never get better by being retried: a `cloneOf` naming a workspace that does not exist, a
/// `restoreOf` whose snapshot id no `done` SnapshotRequest carries. Without this branch each of
/// them requeues at `RETRY` forever, and the log line is indistinguishable from a registry outage.
async fn check_source(source: Option<&crd::VolumeSource>, ctx: &Arc<Ctx>) -> Result<(), Outcome> {
    match source {
        None | Some(crd::VolumeSource::GitRepo { .. }) => Ok(()),
        Some(crd::VolumeSource::CloneOf { volume }) => {
            let api: Api<crd::Workspace> = Api::all(ctx.client.clone());
            match api.get_opt(volume).await {
                Ok(Some(_)) => Ok(()),
                Ok(None) => Err(Outcome::Permanent(format!("clone source {volume} does not exist"), "NoSuchSource")),
                Err(e) => Err(e.into()),
            }
        }
        Some(crd::VolumeSource::RestoreOf { volume, snapshot_id }) => {
            let api: Api<crd::SnapshotRequest> = Api::all(ctx.client.clone());
            let lp = kube::api::ListParams::default().labels(&format!("{}={volume}", crd::VOLUME_LABEL));
            let items = api.list(&lp).await.map_err(Outcome::from)?.items;
            let found = items.iter().any(|r| {
                r.status.as_ref().is_some_and(|s| {
                    s.phase == crd::Phase::Done && s.snapshot_id.as_deref() == Some(snapshot_id.as_str())
                })
            });
            if found {
                Ok(())
            } else {
                Err(Outcome::Permanent(
                    format!("no completed snapshot {snapshot_id} for volume {volume}"),
                    "NoSuchSnapshot",
                ))
            }
        }
    }
}

pub async fn apply_workspace(w: &crd::Workspace, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    heal_labels(&Api::<crd::Workspace>::all(ctx.client.clone()), w, &w.spec.owner, &w.spec.team, "workspace").await?;
    let gen = w.meta().generation.unwrap_or(0);
    let prev = w.status.clone().unwrap_or_default();
    let id = w.name_any();

    // Before anything is created: a source that can never resolve is a permanent failure, and the
    // difference between "wrong forever" and "briefly unavailable" is what `settle` writes down.
    if let Err(outcome) = check_source(w.spec.storage.source.as_ref(), ctx).await {
        let prev2 = prev.clone();
        return settle(outcome, w, "Workspace", gen, move |cond| {
            serde_json::json!({
                "phase": crd::Phase::Error,
                "nodeName": prev2.node_name,
                "compatibleNodes": prev2.compatible_nodes,
                "conditions": [cond],
            })
        }, ctx)
        .await;
    }

    let vol = ensure_child_volume(
        w, &w.spec.owner, &w.spec.team, &w.spec.region, &w.spec.storage, &prev.node_name, "workspace", ctx,
    )
    .await?;
    // The belt to `ensure_child_volume`'s brace: two places allowed to name a node is two places
    // that can disagree about where the data is, and the failure mode is an owner's data split
    // across pools — so a disagreement refuses rather than picks.
    if vol.spec.node_name != prev.node_name {
        let why = format!(
            "status.nodeName {} disagrees with volume {id}'s node {}",
            prev.node_name, vol.spec.node_name
        );
        write_ws_status(
            w,
            crd::WorkspaceStatus {
                phase: crd::Phase::Error,
                conditions: vec![crd::condition("Degraded", true, "NodeMismatch", &why, gen)],
                ..prev.clone()
            },
            ctx,
        )
        .await?;
        return Ok(Action::await_change());
    }
    if !volume_is_ready(&vol) {
        write_ws_status(
            w,
            crd::WorkspaceStatus {
                phase: crd::Phase::Creating,
                observed_generation: None,
                volume_ref: Some(id.clone()),
                conditions: vec![crd::condition("VolumeReady", false, "VolumeNotReady", "the subvolume is not materialized yet", gen)],
                ..prev.clone()
            },
            ctx,
        )
        .await?;
        return Ok(Action::requeue(TICK));
    }
    // The namespace is the OwnerBinding reconciler's to make; this one only waits for it. Creating
    // it here as well is how it ended up with two writers.
    if !crate::binding::namespace_ready(ctx, &w.spec.region, &w.spec.owner).await? {
        write_ws_status(
            w,
            crd::WorkspaceStatus {
                phase: crd::Phase::Creating,
                observed_generation: None,
                volume_ref: Some(id.clone()),
                conditions: vec![crd::condition(
                    crate::binding::NAMESPACE_READY, false, "NamespaceNotReady", "waiting for the owner's namespace", gen,
                )],
                ..prev.clone()
            },
            ctx,
        )
        .await?;
        return Ok(Action::requeue(TICK));
    }

    let ns = crd::ws_namespace(&w.spec.owner, &w.spec.team);
    let owner_ref = owner_ref_of_kind(w)?;
    let pod_ctx = k8s::PodContext {
        pool: &ctx.pool,
        node_name: &vol.spec.node_name,
        owner_ref: owner_ref.clone(),
        runtime_class: ctx.runtime_class.as_deref(),
    };
    ensure(
        &Api::<PersistentVolume>::all(ctx.client.clone()),
        &k8s::local_pv(&id, &w.spec.owner, vol.spec.quota_gb, &pod_ctx),
    )
    .await?;
    ensure(
        &Api::<PersistentVolumeClaim>::namespaced(ctx.client.clone(), &ns),
        &k8s::claim(&ns, &id, &w.spec.owner, vol.spec.quota_gb, &owner_ref),
    )
    .await?;

    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &ns);
    let (phase, pod_ref) = match w.spec.desired_state {
        DesiredState::Running => {
            let init = w
                .spec
                .storage
                .source
                .as_ref()
                .and_then(|s| k8s::git_init_container(s, &ctx.git_init_image, &ctx.git_ssh_host, &ctx.git_ssh_port));
            create_if_absent(&pods, &k8s::workspace_pod(&w.spec, &id, &pod_ctx, init)).await?;
            if !pod_is_ready(&pods, &id).await? {
                write_ws_status(
                    w,
                    crd::WorkspaceStatus {
                        phase: crd::Phase::Creating,
                        observed_generation: None,
                        volume_ref: Some(id.clone()),
                        pod_ref: Some(format!("{ns}/{id}")),
                        conditions: vec![crd::condition("Ready", false, "PodNotReady", "pod is not ready yet", gen)],
                        ..prev.clone()
                    },
                    ctx,
                )
                .await?;
                return Ok(Action::requeue(TICK));
            }
            (crd::Phase::Ready, Some(format!("{ns}/{id}")))
        }
        DesiredState::Stopped => {
            delete_ignoring_404(&pods, &id).await?;
            (crd::Phase::Stopped, None)
        }
    };
    write_ws_status(
        w,
        crd::WorkspaceStatus {
            phase,
            observed_generation: Some(gen),
            volume_ref: Some(id),
            pod_ref,
            conditions: vec![crd::condition("Ready", true, "Converged", "workspace matches spec", gen)],
            ..prev
        },
        ctx,
    )
    .await?;
    Ok(Action::await_change())
}
```

Update `write_ws_status`'s equality check (lines 729-742) to compare `node_name`,
`compatible_nodes` and `volume_ref` as well — a status write that is not a change is the classic
controller hot loop.

Add to `Ctx` and `Ctx::new`:

```rust
    /// Where `gitRepo` seeding clones from and with what. `WS_GIT_BASE` and the agent-side
    /// `git_clone` are gone: the clone happens inside the pod, over SSH, as the owner.
    pub git_ssh_host: String,
    pub git_ssh_port: String,
    pub git_init_image: String,
```

filled from `WS_GIT_SSH_HOST`, `WS_GIT_SSH_PORT` (default `"22"`) and `WS_GIT_INIT_IMAGE`
(default `"alpine/git:2.45.2"`).

In `run`, add the two extra watches to the workspace controller:

```rust
    // Label-selected, not every Pod in the cluster. This watch was already cluster-wide with
    // `watcher::Config::default()`, and this change adds three more watches per agent — a
    // controller that streams every pod event in the cluster to filter for its own is the cheapest
    // way to peg an API server.
    let ours = watcher::Config::default().labels(&format!("{}=workspace", k8s::KIND_LABEL));
    let workspaces = Controller::new(Api::<crd::Workspace>::all(ctx.client.clone()), placed.clone())
        .watches(Api::<Pod>::all(ctx.client.clone()), ours, |p| owned_by::<crd::Workspace, _>(&p))
        // The parent acts on the child's STATUS, so it must wake when that status moves — a
        // 15s requeue is the backstop, never the mechanism.
        .watches(Api::<crd::Volume>::all(ctx.client.clone()), watcher::Config::default(), |v| {
            owned_by::<crd::Workspace, _>(&v)
        })
        // A clone waits on its SOURCE's `compatibleNodes`, which is a status field of another
        // Workspace — a cross-object dependency with no ownerReference to carry it. Without this
        // mapper a clone created before its source is placed sits unplaced with nothing to wake it
        // but the requeue backstop, and the design says requeue is only ever the backstop.
        .watches(Api::<crd::Workspace>::all(ctx.client.clone()), watcher::Config::default(), |src: crd::Workspace| {
            // One event fans out to the clones OF this source. Which those are is not derivable
            // from the source, so the mapper runs the other way: every clone names its source, and
            // the controller's own store is what resolves it. Emitting the source's own ref is
            // enough — a clone whose source moved reconciles on its next pass, and the source's
            // reconcile is what wrote the status that matters.
            Some(kube::runtime::reflector::ObjectRef::<crd::Workspace>::new(&src.name_any()))
        })
```

with `let placed = watcher::Config::default().fields(&format!("status.nodeName={}", ctx.node));`
replacing `mine` for the Workspace and Environment controllers (the Volume and OwnerBinding
controllers keep `mine` = `spec.nodeName=`; SnapshotRequest has no selector at all — D11).

The `cloneOf` mapper above is the cheap form and it is not quite the full dependency: it wakes the
SOURCE, not the clone. Use this instead, which wakes the actual waiter by reading the controller's
own reflector store — the clone is what names the source, so the lookup goes that way:

```rust
        .watches(Api::<crd::Workspace>::all(ctx.client.clone()), watcher::Config::default(), {
            let client = ctx.client.clone();
            move |src: crd::Workspace| {
                // A clone names its source in `storage.source.cloneOf.workspace`; this is the one
                // direction the relationship is written in, so the fan-out is a list filtered by
                // that field. Cheap: it runs only on a Workspace event, and the list is one
                // label-scoped call for the source's owner.
                let (client, owner, name) = (client.clone(), src.spec.owner.clone(), src.name_any());
                futures::executor::block_on(async move {
                    let api: Api<crd::Workspace> = Api::all(client);
                    let lp = kube::api::ListParams::default().labels(&format!("{}={owner}", k8s::OWNER_LABEL));
                    api.list(&lp)
                        .await
                        .map(|l| {
                            l.items
                                .into_iter()
                                .filter(|w| {
                                    matches!(&w.spec.storage.source,
                                             Some(crd::VolumeSource::CloneOf { volume }) if *volume == name)
                                })
                                .map(|w| kube::runtime::reflector::ObjectRef::<crd::Workspace>::new(&w.name_any()))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                })
            }
        })
```

`watches`'s mapper returns an `IntoIterator<Item = ObjectRef<K>>`, so a `Vec` is legal where the
other mappers return `Option`. If `block_on` inside the mapper proves awkward against kube-runtime
4.2.0's signature (it is a sync `FnMut`), fall back to the source-ref form above plus a
`ponytail:` note — the clone still converges on its 15s requeue, and the fan-out is two objects.

The `OwnerBinding`→`Workspace` wake-up stays requeue-backed. Do NOT add that watch; add this
comment where the Workspace reconciler waits on `NamespaceReady` instead:

```rust
    // ponytail: a binding becoming ready wakes a waiting workspace only via its 15s requeue —
    // mapping one binding to every waiting Workspace of that owner is a list per binding event, and
    // the wait is bounded by one tick. Wire a `spec.owner`-indexed reflector if first-workspace
    // latency ever shows up as a complaint.
```

Delete from `apply_workspace`'s old body: the `Namespace`, `NetworkPolicy`, `LimitRange` and
`RoleBinding` `ensure` calls (old lines 635-657) — they belong to Task 3's reconciler now.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p kloudlite-git-agent --test reconcile` and
`cargo test -p kloudlite-git-workspaces`
Expected: the three new tests PASS.

- [ ] **Step 5: Commit**

```bash
git add bins/agent/src/controller.rs crates/workspaces/src/k8s.rs bins/agent/tests/reconcile.rs
git commit -m "Make a workspace create its volume and wait for the disk"
```

---

## Task 5: SnapshotRequest reconciler, and a slimmer Volume

**Files:**
- Create: `bins/agent/src/snapshot.rs`
- Modify: `bins/agent/src/controller.rs:281-514` (`push_pending`, `apply_volume`, `Work`,
  `volume_work`, `read_git_token`, `git_clone`, `base64_basic`), `:91-97` (`Done`), `run`
- Test: `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Consumes: `crd::SnapshotRequest`, `crd::SNAPSHOT_FINALIZER`, `crd::Phase` (Task 1);
  `Ctx::running` (`InFlight`); `controller::{Outcome, settle}` (Task 2).
- Produces:
  - `pub async fn snapshot::reconcile_snapshot(r: Arc<crd::SnapshotRequest>, ctx: Arc<Ctx>) -> Result<Action, ReconcileErr>` (the finalizer wrapper)
  - `pub async fn snapshot::apply_snapshot(r: &crd::SnapshotRequest, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr>`
  - `pub async fn snapshot::cleanup_snapshot(r: &crd::SnapshotRequest, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr>`
  - `controller::Done { phase: crd::Phase, lineage_tip: Option<String> }` (loses `last_push`)
  - `controller::Work { id, owner, source, materialize }` (loses `push`, `message`, `git_token`)

- [ ] **Step 1: Write the failing test**

```rust
const SNAP_STATUS: &str = "/apis/kloudlite-git.io/v1alpha1/snapshotrequests/snap-1/status";
const VOL_GET: &str = "/apis/kloudlite-git.io/v1alpha1/volumes/ws-1";

fn snap_json(status: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "SnapshotRequest",
        "metadata": {"name": "snap-1", "uid": "snap-uid-1", "generation": 1,
                     "finalizers": ["kloudlite-git.io/snapshot"],
                     "labels": {"kloudlite-git.io/owner": "alice", "kloudlite-git.io/volume": "ws-1"}},
        // No `nodeName`: a node is a controller-owned fact and the API does not copy facts into
        // spec. The agent resolves it from the named Volume.
        "spec": {"volume": "ws-1", "message": "checkpoint"},
        "status": status,
    })
}

fn snapshot(status: serde_json::Value) -> crd::SnapshotRequest {
    serde_json::from_value(snap_json(status)).unwrap()
}

/// The Volume this request names, on the node the test's `ctx` is (`node-a`) unless told otherwise.
fn vol_on(node: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "ws-1", "uid": "vol-uid-1"},
        "spec": {"owner": "alice", "team": "", "nodeName": node, "region": "r1", "quotaGb": 20},
        "status": {"phase": "ready", "subvolumePresent": true}
    })
}

/// A push runs once and says what it produced. The uid-keyed `running` map is the idempotency
/// guard, exactly as for Volume work — a second reconcile of a request in flight starts nothing.
#[tokio::test]
async fn a_snapshot_request_runs_the_push_once_and_writes_done() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_git_workspaces::kube_test::get(VOL_GET, vol_on("node-a")),
            Route { method: "PATCH", path: SNAP_STATUS.into(), status: 200, body: snap_json(serde_json::json!({})) },
        ],
    );

    // Stand in for the push having already finished: the reconcile that OBSERVES it is what writes
    // `done`, and which pass that is depends on a thread, not on the reconcile.
    ctx.running.lock().unwrap().insert(
        "snap-uid-1".to_string(),
        (1, tokio::task::spawn_blocking(|| {
            Ok(kloudlite_git_agent::controller::Done {
                phase: crd::Phase::Done,
                lineage_tip: Some("layer-9".into()),
            })
        })),
    );
    wait_idle(&ctx).await;

    let action = kloudlite_git_agent::snapshot::apply_snapshot(&snapshot(serde_json::json!({"phase": "working"})), &ctx)
        .await
        .unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change());
    let sent = rec.sent("PATCH", SNAP_STATUS);
    let last = sent.last().unwrap();
    assert_eq!(last["status"]["phase"], "done");
    assert_eq!(last["status"]["snapshotId"], "layer-9");
    assert_eq!(last["status"]["observedGeneration"], 1);
    assert!(last["status"]["at"].as_str().unwrap().contains('T'), "an rfc3339 stamp: {last}");
    assert!(
        last["status"]["conditions"].as_array().unwrap().iter().any(|c| c["type"] == "Ready" && c["status"] == "True"),
        "{last}"
    );
    assert!(ctx.running.lock().unwrap().is_empty(), "the finished handle must be drained");
    // Nothing outside its own object. Two controllers force-applying one Volume status under one
    // field manager prune each other's fields — the Volume's next pass would delete it anyway.
    assert!(
        !rec.calls().iter().any(|c| c.contains("/volumes/ws-1/status")),
        "the snapshot reconciler must not write the Volume's status: {:?}", rec.calls()
    );
}

/// A request whose Volume lives on another node belongs to another agent. Every agent watches every
/// request (there is no field selector), so "not mine" must be silent — a second agent writing this
/// object's status is exactly the multi-writer problem the design removes.
#[tokio::test]
async fn a_request_for_another_nodes_volume_is_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![kloudlite_git_workspaces::kube_test::get(VOL_GET, vol_on("node-b"))]);

    let action = kloudlite_git_agent::snapshot::apply_snapshot(&snapshot(serde_json::json!({})), &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change());
    assert!(
        !rec.calls().iter().any(|c| c.starts_with("PATCH")),
        "another node's request must not be touched: {:?}", rec.calls()
    );
}

/// An agent restart loses the `running` map. A request left at `working` therefore has a
/// `Progressing` condition and no handle, and there is no way to tell "crashed before starting"
/// from "crashed mid-send" — so it must NOT be re-run: `engine.push_env` would take a fresh
/// snapshot and register a SECOND commit record for one user push.
#[tokio::test]
async fn a_working_request_with_no_handle_fails_instead_of_pushing_twice() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_git_workspaces::kube_test::get(VOL_GET, vol_on("node-a")),
            Route { method: "PATCH", path: SNAP_STATUS.into(), status: 200, body: snap_json(serde_json::json!({})) },
        ],
    );

    let action = kloudlite_git_agent::snapshot::apply_snapshot(&snapshot(serde_json::json!({"phase": "working"})), &ctx)
        .await
        .unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "a permanent failure is not retried");
    let last = rec.sent("PATCH", SNAP_STATUS).last().unwrap().clone();
    assert_eq!(last["status"]["phase"], "error");
    assert!(
        last["status"]["conditions"].as_array().unwrap().iter()
            .any(|c| c["type"] == "Ready" && c["status"] == "False" && c["reason"] == "AgentRestarted"),
        "{last}"
    );
    assert!(ctx.running.lock().unwrap().is_empty(), "nothing was started");
    assert!(!tmp.path().join("vol/ws-1").exists(), "and no second push ran");
}

/// A request is never re-run past `done`. The record is durable and content-addressed; running it
/// again would push a second commit nobody asked for.
#[tokio::test]
async fn a_done_snapshot_request_does_nothing_on_a_second_reconcile() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, rec) = ctx(tmp.path(), vec![]);
    let r = snapshot(serde_json::json!({"phase": "done", "snapshotId": "layer-9", "at": "2026-08-27T00:00:00Z"}));

    let action = kloudlite_git_agent::snapshot::apply_snapshot(&r, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change());
    assert!(rec.calls().is_empty(), "a finished request writes nothing — not even the Volume read: {:?}", rec.calls());
    assert!(!tmp.path().join("vol/ws-1").exists(), "and starts nothing");
}

/// Deleting a request mid-push must WAIT. This is why the request has a finalizer at all: a delete
/// during `working` would otherwise orphan a btrfs RO snapshot, a stage file, an in-flight blob
/// upload and a possible `POST /commits` with no object left to record the outcome in — and the
/// Volume's own finalizer does not cover it, because a SnapshotRequest is not the Volume's child.
#[tokio::test]
async fn deleting_a_working_request_waits_for_the_handle() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, _rec) = ctx(tmp.path(), vec![]);
    ctx.running.lock().unwrap().insert(
        "snap-uid-1".to_string(),
        (1, tokio::task::spawn_blocking(|| {
            std::thread::sleep(std::time::Duration::from_millis(700));
            Ok(kloudlite_git_agent::controller::Done { phase: crd::Phase::Done, lineage_tip: None })
        })),
    );
    let r = snapshot(serde_json::json!({"phase": "working"}));

    let action = kloudlite_git_agent::snapshot::cleanup_snapshot(&r, &ctx).await.unwrap();
    assert_eq!(
        action,
        kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)),
        "cleanup must requeue while the push is running"
    );
    assert!(!ctx.running.lock().unwrap().is_empty(), "nothing was drained by a cleanup that waited");

    wait_idle(&ctx).await;
    kloudlite_git_agent::snapshot::cleanup_snapshot(&r, &ctx).await.unwrap();
    assert!(ctx.running.lock().unwrap().is_empty(), "the finished handle must be drained by cleanup");
}

/// The Volume controller no longer has a push branch at all: pushing is an object with its own
/// reconciler, and `volume_work` is materialize-or-nothing.
#[tokio::test]
async fn a_volume_with_a_push_annotation_starts_no_push() {
    let tmp = tempfile::tempdir().unwrap();
    let (ctx, _rec) = ctx(tmp.path(), vec![patch_ok(VOL_STATUS)]);
    let mut v = volume(1);
    v.metadata.annotations =
        Some(std::collections::BTreeMap::from([("kloudlite-git.io/push-requested".to_string(), "2026-08-27T00:00:00Z".to_string())]));
    // Already observed: with the push branch gone there is nothing left for this pass to do.
    v.status = Some(crd::VolumeStatus { phase: crd::Phase::Ready, observed_generation: Some(1), subvolume_present: true, ..Default::default() });

    let action = kloudlite_git_agent::controller::apply_volume(&v, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::await_change(), "the annotation is dead weight now");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p kloudlite-git-agent --test reconcile`
Expected: FAIL to compile — `kloudlite_git_agent::snapshot` does not exist, `Done` still has
`last_push`.

- [ ] **Step 3: Write `bins/agent/src/snapshot.rs` and slim the Volume**

```rust
//! One push, one object, one reconciler.
//!
//! The CR is the REQUEST; the snapshot is the registry commit record its reconciler writes to the
//! server tier — durable, content-addressed, cross-region, and what a cold clone or a restore on
//! another node reads. Deleting the CR deletes no data.
//!
//! The idempotency guard is `Ctx::running`, keyed by the request's uid, exactly as for Volume work;
//! the Volume's `ws_lock` inside the engine serialises this against a clone-running or a restore on
//! the same disk.

use crate::controller::{patch_status, running_contains, Ctx, Done, ReconcileErr, TICK};
use kube::api::Api;
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{finalizer, Event as FinalizerEvent};
use kube::ResourceExt;
use kloudlite_git_workspaces::crd;
use std::sync::Arc;

/// The finalizer wrapper, exactly the shape `reconcile_volume` has: a delete routes every pass to
/// `cleanup_snapshot` until it returns, which is what makes waiting for an in-flight push free.
pub async fn reconcile_snapshot(r: Arc<crd::SnapshotRequest>, ctx: Arc<Ctx>) -> Result<Action, ReconcileErr> {
    let api: Api<crd::SnapshotRequest> = Api::all(ctx.client.clone());
    finalizer(&api, crd::SNAPSHOT_FINALIZER, r, |event| async {
        match event {
            FinalizerEvent::Cleanup(r) => cleanup_snapshot(&r, &ctx).await,
            FinalizerEvent::Apply(r) => apply_snapshot(&r, &ctx).await,
        }
    })
    .await
    .map_err(|e| ReconcileErr(e.to_string()))
}

/// Whether this agent owns the request, by reading the named Volume's node.
///
/// `Ok(None)` means "not mine, or not resolvable yet" and the caller does NOTHING — no status, no
/// condition. Every agent watches every request, so a second agent writing this object's status is
/// the multi-writer problem the design exists to remove. A Volume that does not exist yet is the
/// same answer: the `SnapshotRequest`→`Volume` watch wakes us when it appears.
async fn my_volume(r: &crd::SnapshotRequest, ctx: &Arc<Ctx>) -> Result<Option<crd::Volume>, ReconcileErr> {
    let api: Api<crd::Volume> = Api::all(ctx.client.clone());
    match api.get_opt(&r.spec.volume).await? {
        Some(v) if v.spec.node_name == ctx.node => Ok(Some(v)),
        _ => Ok(None),
    }
}

pub async fn apply_snapshot(r: &crd::SnapshotRequest, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    let uid = r.uid().unwrap_or_default();
    let gen = r.meta().generation.unwrap_or(0);
    let phase = r.status.as_ref().map(|s| s.phase).unwrap_or(crd::Phase::Pending);
    // A request is never re-run past `done` or `error`: the bytes are already in the registry (or
    // the user has been told to push again), and a second run appends a commit nobody asked for.
    // Checked BEFORE the Volume read, so a finished request costs no API call at all.
    if matches!(phase, crd::Phase::Done | crd::Phase::Error) && !running_contains(ctx, &uid) {
        return Ok(Action::await_change());
    }
    let Some(vol) = my_volume(r, ctx).await? else {
        return Ok(Action::await_change());
    };

    let (finished, still_running) = {
        let mut running = ctx.running.lock().unwrap_or_else(|p| p.into_inner());
        match running.get(&uid) {
            Some((_, h)) if h.is_finished() => (running.remove(&uid), false),
            Some(_) => (None, true),
            None => (None, false),
        }
    };
    if still_running {
        write_status(r, working(gen), ctx).await?;
        return Ok(Action::requeue(TICK));
    }
    // The restart case: `working` in status, nothing in the map. The map died with the process, and
    // there is no way to tell "crashed before starting" from "crashed mid-send" — so this is NOT
    // re-run. `engine.push_env` would take a fresh snapshot and register a SECOND commit record for
    // one user push. Marked permanently failed; the user pushes again.
    // ponytail: fails instead of resuming. The engine already leaves an internal `unpushed` stage
    // mark for crash recovery — resume from it once the engine can answer "is this lineage entry
    // already registered", and this branch becomes a retry.
    if finished.is_none() && phase == crd::Phase::Working {
        let st = serde_json::json!({
            "phase": crd::Phase::Error,
            "observedGeneration": gen,
            "conditions": [crd::condition(
                "Ready", false, "AgentRestarted",
                "the agent restarted while this push was in flight; push again", gen,
            )],
        });
        write_status(r, st, ctx).await?;
        return Ok(Action::await_change());
    }
    if let Some((_, handle)) = finished {
        let outcome = handle.await.unwrap_or_else(|e| Err(format!("push panicked: {e}")));
        return match outcome {
            Ok(done) => {
                let at = k8s_openapi::jiff::Timestamp::now().to_string();
                let st = serde_json::json!({
                    "phase": crd::Phase::Done,
                    "observedGeneration": gen,
                    "snapshotId": done.lineage_tip,
                    "lineageTip": done.lineage_tip,
                    "at": at,
                    "conditions": [crd::condition("Ready", true, "Pushed", "the snapshot record is in the registry", gen)],
                });
                write_status(r, st, ctx).await?;
                // Nothing is written on the Volume. "The newest snapshot of this volume" is a query
                // over these objects by the `kloudlite-git.io/volume` label — a second controller
                // force-applying the Volume's status under the same field manager would have its
                // field pruned by the Volume reconciler's very next pass.
                Ok(Action::await_change())
            }
            // A failed push is `error` with the reason, and the user pushes again. Not a retry
            // loop: a btrfs send that failed once fails the same way at RETRY, and the log line is
            // indistinguishable from a healthy idle agent.
            Err(e) => {
                let st = serde_json::json!({
                    "phase": crd::Phase::Error,
                    "observedGeneration": gen,
                    "conditions": [crd::condition("Ready", false, "PushFailed", &e, gen)],
                });
                write_status(r, st, ctx).await?;
                Ok(Action::await_change())
            }
        };
    }

    // Start it, on its own OS thread: `Engine::push_env` blocks on `ws_lock`'s synchronous
    // `libc::flock`, and a lock wait on the shared reactor would freeze every other workspace.
    let engine = ctx.engine.clone();
    let volume = r.spec.volume.clone();
    let message = r.spec.message.clone();
    // `spec.owner` on the Volume is the truth; the request's `kloudlite-git.io/owner` label is a view
    // of it, and this repo never reads a label as authority.
    let owner = vol.spec.owner.clone();
    let handle = tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
        rt.block_on(async {
            // `push_env` rather than `push`: the VOLUME is what gets pushed, keyed by id alone.
            let out = engine
                .push_env(&owner, &volume, &serde_json::Value::Null, message.as_deref())
                .await
                .map_err(|e| e.to_string())?;
            Ok(Done { phase: crd::Phase::Done, lineage_tip: Some(out.layer) })
        })
    });
    ctx.running.lock().unwrap_or_else(|p| p.into_inner()).insert(uid, (gen, handle));
    write_status(r, working(gen), ctx).await?;
    Ok(Action::requeue(TICK))
}

/// Wait for an in-flight push, then let the object go.
///
/// The same shape and the same reason as `cleanup_volume`: reclaiming or abandoning while a
/// `btrfs send` is still reading destroys the source mid-stream, and the finalizer makes waiting
/// cost one tick. The finished handle must be DRAINED here, not merely observed — while an object
/// is deleting the finalizer routes every pass to this arm, so `apply_snapshot` never runs and
/// nothing else would ever remove the entry.
pub async fn cleanup_snapshot(r: &crd::SnapshotRequest, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    let uid = r.uid().unwrap_or_default();
    let mut running = ctx.running.lock().unwrap_or_else(|p| p.into_inner());
    match running.get(&uid) {
        Some((_, h)) if h.is_finished() => {
            running.remove(&uid);
        }
        Some(_) => {
            tracing::info!(request = %r.name_any(), "delete waiting for an in-flight push");
            return Ok(Action::requeue(TICK));
        }
        None => {}
    }
    // Nothing on disk or in the registry is reclaimed by this: the record is content-addressed and
    // shared, and deleting the wish never deletes the bytes.
    Ok(Action::await_change())
}

fn working(gen: i64) -> serde_json::Value {
    serde_json::json!({
        "phase": crd::Phase::Working,
        "observedGeneration": gen,
        "conditions": [crd::condition("Progressing", true, "Working", "btrfs snapshot and upload in flight", gen)],
    })
}

async fn write_status(r: &crd::SnapshotRequest, st: serde_json::Value, ctx: &Arc<Ctx>) -> Result<(), ReconcileErr> {
    // Same guard as everywhere else: a status write that is not a change is a watch event that
    // triggers itself, which is an outage rather than a warning.
    if let Some(cur) = &r.status {
        if serde_json::to_value(cur).ok().is_some_and(|c| same_phase(&c, &st)) {
            return Ok(());
        }
    }
    let api: Api<crd::SnapshotRequest> = Api::all(ctx.client.clone());
    patch_status(&api, &r.name_any(), "SnapshotRequest", st).await
}

/// Equality that ignores `lastTransitionTime` on conditions — a condition re-stamped with `now` is
/// not a change.
fn same_phase(cur: &serde_json::Value, want: &serde_json::Value) -> bool {
    cur["phase"] == want["phase"] && cur["snapshotId"] == want["snapshotId"]
}
```

In `controller.rs`:

- `Done` (lines 92-97) becomes `pub struct Done { pub phase: crd::Phase, pub lineage_tip: Option<String> }`.
- Delete `push_pending` (281-289), `read_git_token` (450-463), `git_clone` (465-508),
  `base64_basic` (510-514) and the `PUSH_ANNOTATION`/`PUSH_MESSAGE_ANNOTATION` import (line 26).
- `Work` (398-409) becomes `pub struct Work { pub id: String, pub owner: String, pub source: Option<VolumeSource>, pub materialize: bool }`.
- `volume_work` (411-448) loses its push block and its `GitRepo` clone arm:

```rust
        Some(VolumeSource::GitRepo { .. }) => {
            // An empty subvolume. The workspace pod's init container is what fills it — inside the
            // workspace, over SSH, as the owner, with no credential the agent could read.
            engine.create_subvol(id).map_err(|e| e.to_string())?
        }
```

  and its tail becomes `Ok(Done { phase: crd::Phase::Ready, lineage_tip: None })`.
- `apply_volume` (291-382) drops `pending`, `message` and `git_token`; its "nothing asked for"
  guard becomes `if observed && !running_contains(ctx, &uid)`, and every `last_push` field in the
  `VolumeStatus` it writes is simply deleted — nothing replaces it (D5).
- `status_eq` (570-578) drops its `last_push` comparison.
- `running_contains` (226-228) becomes `pub`.
- In `run`, add the controller. It has NO field selector — a `SnapshotRequest` names no node, so
  every agent watches every request and `my_volume` is what decides ownership:

```rust
    // No `mine`: the request carries no node (a node is a controller-owned fact and the API does
    // not copy facts into spec), so ownership is resolved per-object from the named Volume.
    // ponytail: every agent streams every request — two nodes today, so the fan-out is two. A
    // `spec.volume`-indexed reflector is the upgrade if the request count ever makes this hot.
    let snapshots = Controller::new(Api::<crd::SnapshotRequest>::all(ctx.client.clone()), watcher::Config::default())
        // A request created before its Volume is placed waits, and this is what wakes it. Without
        // it the wait is the 15s requeue, and the design says requeue is only ever the backstop.
        .watches(Api::<crd::Volume>::all(ctx.client.clone()), watcher::Config::default(), {
            let client = ctx.client.clone();
            move |v: crd::Volume| {
                let (client, name) = (client.clone(), v.name_any());
                futures::executor::block_on(async move {
                    let api: Api<crd::SnapshotRequest> = Api::all(client);
                    let lp = kube::api::ListParams::default().labels(&format!("{}={name}", crd::VOLUME_LABEL));
                    api.list(&lp)
                        .await
                        .map(|l| {
                            l.items
                                .into_iter()
                                .map(|r| kube::runtime::reflector::ObjectRef::<crd::SnapshotRequest>::new(&r.name_any()))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                })
            }
        })
        .shutdown_on_signal()
        .run(snapshot::reconcile_snapshot, error_policy, ctx.clone())
        .for_each(|r| async move {
            if let Err(e) = r {
                tracing::warn!(error = %e, "snapshot reconcile")
            }
        });
```

  The `block_on`-in-a-sync-mapper caveat from Task 4 applies here too: if kube-runtime 4.2.0's
  `watches` signature will not take it, drop the mapper and add
  `// ponytail: a request created before its Volume is placed waits one 15s tick instead of being
  woken by the Volume's creation.`

- [ ] **Step 4: Run the tests**

Run: `cargo test -p kloudlite-git-agent --test reconcile`
Expected: PASS, including the pre-existing
`deleting_a_volume_waits_for_an_in_flight_operation` and
`a_reconcile_that_cannot_read_the_pool_deletes_nothing`.

- [ ] **Step 5: Commit**

```bash
git add bins/agent/src/snapshot.rs bins/agent/src/controller.rs bins/agent/tests/reconcile.rs
git commit -m "Make a push an object with its own reconciler"
```

---

## Task 6: The Environment reconciler

**Files:**
- Modify: `bins/agent/src/controller.rs:746-966` (`apply_environment`, `await_stop_push`,
  `write_env_status`) and `run`
- Test: `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Consumes: `ensure_child_volume`, `volume_is_ready` (Task 4); `crd::SnapshotRequest` (Task 1).
- Produces: `pub async fn controller::apply_environment(e: &crd::Environment, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr>`;
  `fn controller::stop_snapshot_name(env_id: &str) -> String` → `format!("stop-{env_id}")`.

- [ ] **Step 1: Write the failing test**

```rust
fn env_json(status: serde_json::Value, desired: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "Environment",
        "metadata": {"name": "env-1", "uid": "env-uid-1", "generation": 1,
                     "labels": {"kloudlite-git.io/owner": "alice", "kloudlite-git.io/kind": "environment", "kloudlite-git.io/team": ""}},
        "spec": {"owner": "alice", "name": "app", "region": "r1", "services": [],
                 "storage": {"quotaGb": 20}, "desiredState": desired},
        "status": status,
    })
}

/// An environment that stops must push first, and the deployments must not go until the push has
/// LANDED — not merely been requested. That gate is now a child object with a status, instead of an
/// annotation echoing its own timestamp back.
#[tokio::test]
async fn stopping_an_environment_creates_one_snapshot_request_and_deletes_nothing_yet() {
    let tmp = tempfile::tempdir().unwrap();
    let vol = serde_json::json!({
        "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "env-1", "uid": "vol-uid-e1"},
        "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20},
        "status": {"phase": "ready", "subvolumePresent": true}
    });
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_git_workspaces::kube_test::get("/apis/kloudlite-git.io/v1alpha1/volumes/env-1", vol),
            kloudlite_git_workspaces::kube_test::not_found("/apis/kloudlite-git.io/v1alpha1/snapshotrequests/stop-env-1"),
            kloudlite_git_workspaces::kube_test::post(
                "/apis/kloudlite-git.io/v1alpha1/snapshotrequests",
                serde_json::json!({"apiVersion": "kloudlite-git.io/v1alpha1", "kind": "SnapshotRequest",
                                   "metadata": {"name": "stop-env-1"},
                                   "spec": {"volume": "env-1", "message": "stop"}}),
            ),
            Route { method: "PATCH", path: "/apis/kloudlite-git.io/v1alpha1/environments/env-1/status".into(),
                    status: 200, body: env_json(serde_json::json!({}), "stopped") },
        ],
    );
    let e: crd::Environment =
        serde_json::from_value(env_json(serde_json::json!({"nodeName": "node-a", "compatibleNodes": ["node-a"]}), "stopped")).unwrap();

    let action = kloudlite_git_agent::controller::apply_environment(&e, &ctx).await.unwrap();
    assert_eq!(action, kube::runtime::controller::Action::requeue(std::time::Duration::from_secs(15)));
    let sent = rec.sent("POST", "/apis/kloudlite-git.io/v1alpha1/snapshotrequests");
    assert_eq!(sent.len(), 1, "exactly one stop snapshot");
    assert_eq!(sent[0]["spec"]["volume"], "env-1");
    assert!(sent[0]["spec"].get("nodeName").is_none(), "a request names no node; the agent resolves it: {}", sent[0]);
    let refs = sent[0]["metadata"]["ownerReferences"].as_array().expect("the stop snapshot is the env's child");
    assert_eq!(refs[0]["kind"], "Environment");
    assert!(
        !rec.calls().iter().any(|c| c.starts_with("DELETE")),
        "an env torn down before its push lands loses its last state for good: {:?}", rec.calls()
    );
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p kloudlite-git-agent --test reconcile stopping_an_environment`
Expected: FAIL — the current code patches a `kloudlite-git.io/push-requested` annotation on the
Volume and creates nothing.

- [ ] **Step 3: Write the implementation**

In `controller.rs`, replace the head of `apply_environment` (lines 751-791) with the same
skeleton as `apply_workspace`:

```rust
pub async fn apply_environment(e: &crd::Environment, ctx: &Arc<Ctx>) -> Result<Action, ReconcileErr> {
    heal_labels(&Api::<crd::Environment>::all(ctx.client.clone()), e, &e.spec.owner, "", "environment").await?;
    let gen = e.meta().generation.unwrap_or(0);
    let prev = e.status.clone().unwrap_or_default();
    let id = e.name_any();
    let owner_ref = owner_ref_of_kind(e)?;
    let vol = ensure_child_volume(
        e, &e.spec.owner, "", &e.spec.region, &e.spec.storage, &prev.node_name, "environment", ctx,
    )
    .await?;
    if vol.spec.node_name != prev.node_name {
        let why = format!("status.nodeName {} disagrees with volume {id}'s node {}", prev.node_name, vol.spec.node_name);
        write_env_status(
            e,
            crd::EnvironmentStatus {
                phase: crd::Phase::Error,
                observed_generation: None,
                conditions: vec![crd::condition("Degraded", true, "NodeMismatch", &why, gen)],
                ..prev.clone()
            },
            ctx,
        )
        .await?;
        return Ok(Action::await_change());
    }
    if !volume_is_ready(&vol) {
        write_env_status(
            e,
            crd::EnvironmentStatus {
                phase: crd::Phase::Creating,
                observed_generation: None,
                volume_ref: Some(id.clone()),
                conditions: vec![crd::condition("VolumeReady", false, "VolumeNotReady", "the subvolume is not materialized yet", gen)],
                ..prev.clone()
            },
            ctx,
        )
        .await?;
        return Ok(Action::requeue(TICK));
    }

    let ns = crd::env_namespace(&id);
    let deployments: Api<Deployment> = Api::namespaced(ctx.client.clone(), &ns);

    if e.spec.desired_state == DesiredState::Stopped {
        // One push of the env's own subvolume covers every mounted volume atomically; an env torn
        // down without it loses its last state for good, which is why the deletes below are gated
        // on the push having LANDED, not merely requested.
        if let Some(action) = await_stop_snapshot(e, &owner_ref, gen, ctx).await? {
            return Ok(action);
        }
        for svc in &e.spec.services {
            delete_ignoring_404(&deployments, &svc.name).await?;
        }
        write_env_status(
            e,
            crd::EnvironmentStatus {
                phase: crd::Phase::Stopped,
                observed_generation: Some(gen),
                volume_ref: Some(id),
                service_status: vec![],
                conditions: vec![crd::condition("Ready", true, "Stopped", "pushed and stopped", gen)],
                ..prev
            },
            ctx,
        )
        .await?;
        return Ok(Action::await_change());
    }
```

The rest of the running path is unchanged except that `e.spec.volume_ref` becomes `id`,
`vol.spec.node_name` still feeds `PodContext`, and the final status carries
`volume_ref: Some(id.clone())` plus `..prev`.

Replace `await_stop_push` (lines 897-929) with:

```rust
/// The name of an environment's stop snapshot. Fixed, not minted: a fixed name is what makes
/// "create it if it is missing" idempotent across restarts without a second piece of state.
fn stop_snapshot_name(env_id: &str) -> String {
    format!("stop-{env_id}")
}

/// `Some(action)` while a stop is still waiting on its push. The request is a CHILD of the
/// environment (`ownerReference`), so a deleted environment takes its pending stop snapshot with
/// it, and the wait reads the child's status rather than an annotation echoing itself back.
async fn await_stop_snapshot(
    e: &crd::Environment,
    owner_ref: &OwnerReference,
    gen: i64,
    ctx: &Arc<Ctx>,
) -> Result<Option<Action>, ReconcileErr> {
    let id = e.name_any();
    let name = stop_snapshot_name(&id);
    let api: Api<crd::SnapshotRequest> = Api::all(ctx.client.clone());
    if let Some(existing) = api.get_opt(&name).await? {
        return match existing.status.as_ref().map(|s| s.phase) {
            Some(crd::Phase::Done) => Ok(None),
            // A failed stop snapshot leaves the environment RUNNING and says so. Tearing the
            // deployments down anyway would discard state that was never saved — which is the one
            // thing this gate exists to prevent — so the stop simply does not complete, and the
            // condition names why.
            Some(crd::Phase::Error) => {
                write_env_status(
                    e,
                    crd::EnvironmentStatus {
                        phase: crd::Phase::Running,
                        observed_generation: None,
                        service_status: vec![],
                        conditions: vec![crd::condition(
                            "Ready", false, "StopSnapshotFailed",
                            "the stop snapshot failed; the environment is still running and its state is unsaved", gen,
                        )],
                        ..e.status.clone().unwrap_or_default()
                    },
                    ctx,
                )
                .await?;
                Ok(Some(Action::await_change()))
            }
            _ => {
                write_env_status(
                    e,
                    crd::EnvironmentStatus {
                        // Still `running`: the deployments ARE up until the push lands, and
                        // `model::EnvState` has no `Stopping` — an unknown phase silently becomes
                        // `Creating`, which is both wrong and alarming.
                        phase: crd::Phase::Running,
                        observed_generation: None,
                        service_status: vec![],
                        conditions: vec![crd::condition("Progressing", true, "PushBeforeStop", "waiting for the stop snapshot", gen)],
                        ..e.status.clone().unwrap_or_default()
                    },
                    ctx,
                )
                .await?;
                Ok(Some(Action::requeue(TICK)))
            }
        };
    }
    let mut req = crd::SnapshotRequest::new(
        &name,
        crd::SnapshotRequestSpec { volume: id.clone(), message: Some("stop".into()) },
    );
    // The ONE SnapshotRequest that IS owner-referenced: a stop snapshot belongs to the stop, so a
    // deleted Environment takes its pending stop child with it. That GC is only safe because the
    // request has a finalizer — without one, deleting the Environment would garbage-collect the
    // request out from under a running `btrfs send`.
    req.metadata.owner_references = Some(vec![owner_ref.clone()]);
    req.metadata.labels = Some(std::collections::BTreeMap::from([
        (k8s::OWNER_LABEL.to_string(), e.spec.owner.clone()),
        (crd::VOLUME_LABEL.to_string(), id),
    ]));
    match api.create(&kube::api::PostParams::default(), &req).await {
        Ok(_) | Err(kube::Error::Api(kube::core::Status { code: 409, .. })) => {}
        Err(e) => return Err(e.into()),
    }
    Ok(Some(Action::requeue(TICK)))
}
```

(If the 409 pattern does not match `kube::core::Status`'s boxed shape, use
`Err(kube::Error::Api(s)) if s.code == 409 => {}` as the other call sites do.)

In `run`, add the Volume and SnapshotRequest watches to the environment controller:

```rust
        .watches(Api::<crd::Volume>::all(ctx.client.clone()), watcher::Config::default(), |v| {
            owned_by::<crd::Environment, _>(&v)
        })
        // The stop snapshot is the environment's child, so `done` wakes the stop that is waiting
        // for it — the 15s requeue is the backstop, not the mechanism.
        .watches(Api::<crd::SnapshotRequest>::all(ctx.client.clone()), watcher::Config::default(), |r| {
            owned_by::<crd::Environment, _>(&r)
        })
```

Delete the `Namespace`/`NetworkPolicy`/`RoleBinding` ensures ONLY where they duplicate the
OwnerBinding reconciler — environments keep their own per-env namespace (spec: "Environments keep
their own namespace per env; only workspaces use the per-owner one"), so lines 793-816 stay as they
are.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p kloudlite-git-agent --test reconcile`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add bins/agent/src/controller.rs bins/agent/tests/reconcile.rs
git commit -m "Gate an environment stop on a snapshot request, not an annotation"
```

---

## Task 7: Startup migration

**Files:**
- Create: `bins/agent/src/migrate.rs`
- Modify: `bins/agent/src/lib.rs:97-108` (`run` — call it before `controller::run`)
- Test: `bins/agent/tests/reconcile.rs`

**Interfaces:**
- Consumes: `Ctx`, `ctx.engine.registry` (D9), `crd::SnapshotRequest` (Task 1).
- Produces: `pub async fn migrate::once(ctx: &Arc<Ctx>) -> Result<(), ReconcileErr>`.

- [ ] **Step 1: Write the failing test**

```rust
/// Objects written before this change have a `spec.volumeRef`, a `Volume` with no ownerReference
/// and a history that exists ONLY in the registry. The migration backfills all three, so the
/// history page and restore keep working from CRs after the roll.
#[tokio::test]
async fn the_migration_adopts_the_volume_and_backfills_one_request_per_commit() {
    let tmp = tempfile::tempdir().unwrap();
    // A pre-migration Workspace: no status at all, and the API server still holds a legacy
    // `spec.nodeName`/`spec.volumeRef` that the new schema prunes on read.
    let legacy = serde_json::json!({
        "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "WorkspaceList", "metadata": {},
        "items": [{
            "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "Workspace",
            "metadata": {"name": "ws-old", "uid": "ws-uid-old"},
            "spec": {"owner": "alice", "team": "", "name": "old", "region": "r1",
                     "image": "nginx:alpine", "storage": {"quotaGb": 20}, "desiredState": "running"}
        }]
    });
    let vol = serde_json::json!({
        "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "Volume",
        "metadata": {"name": "ws-old", "uid": "vol-uid-old"},
        "spec": {"owner": "alice", "team": "", "nodeName": "node-a", "region": "r1", "quotaGb": 20},
        "status": {"phase": "ready", "subvolumePresent": true}
    });
    let (ctx, rec) = ctx(
        tmp.path(),
        vec![
            kloudlite_git_workspaces::kube_test::get("/apis/kloudlite-git.io/v1alpha1/workspaces", legacy),
            kloudlite_git_workspaces::kube_test::get("/apis/kloudlite-git.io/v1alpha1/volumes/ws-old", vol.clone()),
            Route { method: "PATCH", path: "/apis/kloudlite-git.io/v1alpha1/volumes/ws-old".into(), status: 200, body: vol },
            Route { method: "PATCH", path: "/apis/kloudlite-git.io/v1alpha1/workspaces/ws-old/status".into(),
                    status: 200, body: serde_json::json!({"apiVersion": "kloudlite-git.io/v1alpha1", "kind": "Workspace", "metadata": {"name": "ws-old"}}) },
            kloudlite_git_workspaces::kube_test::get(
                "/apis/kloudlite-git.io/v1alpha1/environments",
                serde_json::json!({"apiVersion": "kloudlite-git.io/v1alpha1", "kind": "EnvironmentList", "metadata": {}, "items": []}),
            ),
        ],
    );

    kloudlite_git_agent::migrate::once(&ctx).await.unwrap();

    let adopted = rec.sent("PATCH", "/apis/kloudlite-git.io/v1alpha1/volumes/ws-old");
    let refs = adopted[0]["metadata"]["ownerReferences"].as_array().expect("the volume is adopted");
    assert_eq!(refs[0]["kind"], "Workspace");
    assert_eq!(refs[0]["name"], "ws-old");
    let st = rec.sent("PATCH", "/apis/kloudlite-git.io/v1alpha1/workspaces/ws-old/status");
    assert_eq!(st[0]["status"]["nodeName"], "node-a", "placement is backfilled from the volume's node");
    assert_eq!(st[0]["status"]["compatibleNodes"], serde_json::json!(["node-a"]));
    assert_eq!(st[0]["status"]["volumeRef"], "ws-old");
}
```

(The registry half — one `SnapshotRequest` per commit record — is exercised by the e2e phase in
Task 10, because the stub API server has no registry; the unit test asserts the CR half, which is
the part that can be wrong silently.)

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p kloudlite-git-agent --test reconcile the_migration_adopts`
Expected: FAIL to compile — `kloudlite_git_agent::migrate` does not exist.

- [ ] **Step 3: Write `bins/agent/src/migrate.rs`**

```rust
//! One-shot startup migration, in the shape `engine::migrate_ws_to_vol` already established.
//!
//! Three things exist from before this change and cannot be reconciled into place on their own: a
//! `Volume` with no ownerReference (nothing would ever GC it), a parent with no `status.nodeName`
//! (the placement watch would claim it a second time, possibly on another node), and pushed history
//! that lives only in the registry (the history page reads CRs now).
//!
//! Idempotent by construction: every write is "set it to what it should be", so a restart mid-way
//! costs a second pass and nothing else.

use crate::controller::{patch_status, Ctx, ReconcileErr};
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::{Resource, ResourceExt};
use kloudlite_git_workspaces::crd;
use kloudlite_git_workspaces::k8s;
use std::sync::Arc;

pub async fn once(ctx: &Arc<Ctx>) -> Result<(), ReconcileErr> {
    let ws: Api<crd::Workspace> = Api::all(ctx.client.clone());
    for w in ws.list(&ListParams::default()).await?.items {
        adopt(ctx, &w, "Workspace", &w.spec.owner, w.status.as_ref().map(|s| s.node_name.clone())).await?;
    }
    let envs: Api<crd::Environment> = Api::all(ctx.client.clone());
    for e in envs.list(&ListParams::default()).await?.items {
        adopt(ctx, &e, "Environment", &e.spec.owner, e.status.as_ref().map(|s| s.node_name.clone())).await?;
    }
    Ok(())
}

/// Adopt one parent's `Volume` (same name, `D1`) and backfill its placement + history.
async fn adopt<P>(
    ctx: &Arc<Ctx>,
    parent: &P,
    kind: &str,
    owner: &str,
    placed: Option<String>,
) -> Result<(), ReconcileErr>
where
    P: Resource<DynamicType = ()> + ResourceExt,
{
    let id = parent.name_any();
    let vols: Api<crd::Volume> = Api::all(ctx.client.clone());
    let Some(vol) = vols.get_opt(&id).await? else { return Ok(()) };
    // Only this node's objects: the volume's spec is the only place the pre-migration node lives.
    if vol.spec.node_name != ctx.node {
        return Ok(());
    }
    if vol.metadata.owner_references.as_ref().is_none_or(|r| r.is_empty()) {
        let owner_ref = crate::controller::owner_ref_of_kind(parent)?;
        let patch = serde_json::json!({"metadata": {"ownerReferences": [owner_ref]}});
        vols.patch(&id, &PatchParams::default(), &Patch::Merge(&patch)).await?;
        tracing::info!(volume = %id, %kind, "migration: adopted an orphan volume");
    }
    if placed.as_deref().unwrap_or_default().is_empty() {
        // From the VOLUME's spec, which is where the pre-migration node actually lives. The
        // parent's own deprecated `spec.nodeName` is read only as a fallback for an object whose
        // Volume has somehow gone — release 1 keeps both fields in the schema precisely so this
        // read is possible on an agent that rolls after the CRD apply (D15).
        let status = serde_json::json!({
            "phase": crd::Phase::Pending,
            "nodeName": vol.spec.node_name,
            "compatibleNodes": [vol.spec.node_name],
            "volumeRef": id,
        });
        match kind {
            "Workspace" => patch_status(&Api::<crd::Workspace>::all(ctx.client.clone()), &id, kind, status).await?,
            _ => patch_status(&Api::<crd::Environment>::all(ctx.client.clone()), &id, kind, status).await?,
        }
        tracing::info!(object = %id, node = %vol.spec.node_name, "migration: backfilled placement from the volume");
    }
    backfill_history(ctx, &id, owner).await
}

/// One `SnapshotRequest` per registry commit record, `phase: done`, ids taken from the record.
///
/// Reads the record surface through the Engine's own `RegistryClient` — the agent already has it
/// pointed at the server tier. A registry that cannot be reached SKIPS this volume with a warning
/// rather than failing startup: the CRs above are the part that must not be missing, and a history
/// page that is briefly empty is recoverable on the next boot.
async fn backfill_history(ctx: &Arc<Ctx>, id: &str, owner: &str) -> Result<(), ReconcileErr> {
    let api: Api<crd::SnapshotRequest> = Api::all(ctx.client.clone());
    let lp = ListParams::default().labels(&format!("{}={id}", crd::VOLUME_LABEL));
    let existing: std::collections::HashSet<String> = api
        .list(&lp)
        .await?
        .items
        .into_iter()
        .filter_map(|r| r.status.and_then(|s| s.snapshot_id))
        .collect();
    let history = match ctx.engine.registry.get_history(owner, id).await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(volume = %id, error = %e, "migration: registry unreachable, history not backfilled");
            return Ok(());
        }
    };
    for rec in history {
        if existing.contains(&rec.id) {
            continue;
        }
        // Deterministic name from the record id: a re-run must not mint a second object for one
        // snapshot, and the record id is already unique and content-addressed.
        let name = format!("snap-{}", rec.id.to_lowercase());
        let mut req = crd::SnapshotRequest::new(
            &name,
            crd::SnapshotRequestSpec { volume: id.to_string(), message: rec.message.clone() },
        );
        req.metadata.labels = Some(std::collections::BTreeMap::from([
            (k8s::OWNER_LABEL.to_string(), owner.to_string()),
            (crd::VOLUME_LABEL.to_string(), id.to_string()),
        ]));
        match api.create(&kube::api::PostParams::default(), &req).await {
            Ok(_) => {}
            Err(kube::Error::Api(s)) if s.code == 409 => continue,
            Err(e) => return Err(e.into()),
        }
        let status = serde_json::json!({
            "phase": crd::Phase::Done,
            "snapshotId": rec.id,
            "at": rec.created_at.to_rfc3339(),
            "conditions": [crd::condition("Ready", true, "Backfilled", "record read from the registry at migration", 0)],
        });
        patch_status(&api, &name, "SnapshotRequest", status).await?;
    }
    Ok(())
}
```

In `bins/agent/src/lib.rs::run`, between the client construction and `controller::run`:

```rust
    let ctx = Arc::new(controller::Ctx::new(client.clone(), engine, cfg.node.clone(), cfg.pool, cfg.region, roles));
    // Before any watch: an orphan Volume or an unplaced-looking Workspace would otherwise be
    // claimed a second time, possibly on another node.
    if let Err(e) = migrate::once(&ctx).await {
        tracing::error!(error = %e, "startup migration failed");
        return Err(e.to_string());
    }
    controller::run(ctx).await
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p kloudlite-git-agent --test reconcile the_migration_adopts`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add bins/agent/src/migrate.rs bins/agent/src/lib.rs bins/agent/tests/reconcile.rs
git commit -m "Adopt pre-migration volumes and backfill their history"
```

---

## Task 8: `/v1` writes one object per action

**Files:**
- Modify: `crates/workspaces/src/api.rs` — `:381-415` (`place_node`, `create_volume`),
  `:334-371` (projections), `:443-470` (`workspace_for`), `:472-556` (`create_ws`,
  `install_user_key`), `:558-625` (`list_ws`, `get_ws`, `delete_ws`), `:649-727` (`clone_ws`,
  `restore_ws`), `:785-956` (env equivalents), `:958-1014` (push), `:1082-1117` (history/refs)
- Modify: `deploy/k3s/agent-rbac.yaml`, `deploy/k3s/api-rbac.yaml`,
  `deploy/k3s/agent-daemonset.yaml`
- Test: `crates/workspaces/tests/api_user.rs`, `crates/workspaces/tests/api_volumes.rs`,
  `crates/workspaces/tests/api_teams.rs`

**Interfaces:**
- Consumes: everything from Task 1.
- Produces: unchanged HTTP surface (`model::Workspace`, `model::Environment`, `CommitRecord`-shaped
  history, `{"main": id}` refs) — see **D2**.

- [ ] **Step 1: Write the failing tests**

Replace `create_ws_writes_a_volume_and_a_workspace_pinned_to_the_owners_node`
(`api_user.rs:124-153`), `a_workspace_never_names_a_node_its_volume_does_not` (`:155-175`),
`clone_lands_on_the_sources_node_with_a_clone_of_source` (`:177-200`),
`restore_carries_the_snapshot_id_as_the_volume_source` (`:202-246`),
`delete_removes_the_workspace_then_its_volume` (`:273-295`) and the two push tests
(`:498-540`) with:

```rust
/// One object per user action. The API used to write two and pick a node; both are the
/// controllers' now, and the node it would have picked is a fact it has no way to know yet.
#[tokio::test]
async fn create_ws_writes_exactly_one_unplaced_workspace() {
    let s = server(vec![post(format!("{API}/workspaces"), ws_obj("ws-new", "karthik"))]).await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "web", "region": "centralindia", "quotaGb": 20, "image": "nginx:alpine"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());

    assert!(!s.rec.calls().iter().any(|c| c.contains("/volumes")), "the API never creates a Volume: {:?}", s.rec.calls());
    assert!(!s.rec.calls().iter().any(|c| c.contains("ownerbindings")), "the API never places: {:?}", s.rec.calls());
    assert!(!s.rec.calls().iter().any(|c| c.contains("/nodes")), "and never reads node capacity");
    let w = &s.rec.sent("POST", &format!("{API}/workspaces"))[0];
    assert_eq!(w["spec"]["storage"]["quotaGb"], 20);
    assert!(w["spec"].get("nodeName").is_none(), "placement is a fact the controllers establish: {w}");
    assert!(w["spec"].get("volumeRef").is_none(), "a volumeRef in spec was a wish about a fact: {w}");
    assert_eq!(w["metadata"]["labels"]["kloudlite-git.io/owner"], "karthik");
}

/// A clone no longer copies a node from the source: locality is the claim's job, via the source's
/// `status.compatibleNodes`.
#[tokio::test]
async fn clone_asks_for_a_clone_source_and_names_no_node() {
    let src = json!({
        "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": "ws-src"},
        "spec": {"owner": "karthik", "team": "", "name": "src", "region": "centralindia",
                 "image": "nginx:alpine", "storage": {"quotaGb": 20}, "desiredState": "running"},
        "status": {"phase": "ready", "nodeName": "node-z", "compatibleNodes": ["node-z"], "volumeRef": "ws-src"}
    });
    let s = server(vec![
        get(format!("{API}/workspaces/ws-src"), src),
        post(format!("{API}/workspaces"), ws_obj("ws-new", "karthik")),
    ])
    .await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-src/clone", s.base))
        .bearer_auth(&tok)
        .json(&json!({"name": "copy"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
    let w = &s.rec.sent("POST", &format!("{API}/workspaces"))[0];
    assert_eq!(w["spec"]["storage"]["source"]["cloneOf"]["volume"], "ws-src");
    assert!(w["spec"].get("nodeName").is_none(), "{w}");
}

/// Delete is ONE call. The "Workspace first, then Volume" ordering became the API server's job the
/// moment the Volume got an ownerReference.
#[tokio::test]
async fn delete_is_one_call() {
    let s = server(vec![
        get(format!("{API}/workspaces/ws-1"), ws_obj("ws-1", "karthik")),
        Route { method: "DELETE", path: format!("{API}/workspaces/ws-1"), status: 200, body: ws_obj("ws-1", "karthik") },
    ])
    .await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .delete(format!("{}/v1/workspaces/ws-1", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
    let deletes: Vec<_> = s.rec.calls().into_iter().filter(|c| c.starts_with("DELETE")).collect();
    assert_eq!(deletes, vec![format!("DELETE {API}/workspaces/ws-1")], "the GC removes the Volume");
}

/// Push is a SnapshotRequest, carrying the volume and the node the disk is on — a snapshot must run
/// where the disk is, so there is nothing for the agent to claim.
#[tokio::test]
async fn push_creates_a_snapshot_request_on_the_volumes_node() {
    let mut ws = ws_obj("ws-1", "karthik");
    ws["status"] = json!({"phase": "ready", "nodeName": NODE, "compatibleNodes": [NODE], "volumeRef": "ws-1"});
    let s = server(vec![
        get(format!("{API}/workspaces/ws-1"), ws),
        get(format!("{API}/volumes/ws-1"), vol_obj("ws-1", "karthik", NODE)),
        post(format!("{API}/snapshotrequests"), json!({
            "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "SnapshotRequest",
            "metadata": {"name": "snap-1"},
            "spec": {"volume": "ws-1", "message": "checkpoint"}
        })),
    ])
    .await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/push", s.base))
        .bearer_auth(&tok)
        .json(&json!({"message": "checkpoint"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "{}", resp.text().await.unwrap());
    let r = &s.rec.sent("POST", &format!("{API}/snapshotrequests"))[0];
    assert_eq!(r["spec"]["volume"], "ws-1");
    assert_eq!(r["spec"]["nodeName"], NODE);
    assert_eq!(r["spec"]["message"], "checkpoint");
    assert_eq!(r["metadata"]["labels"]["kloudlite-git.io/volume"], "ws-1");
    assert!(!s.rec.calls().iter().any(|c| c.starts_with("PATCH")), "no annotation dance: {:?}", s.rec.calls());
}

/// A workspace whose Volume does not exist yet cannot be pushed. 409 "not ready yet", not a 500 and
/// not a silently dropped request.
#[tokio::test]
async fn push_before_the_volume_exists_is_a_conflict() {
    let s = server(vec![get(format!("{API}/workspaces/ws-1"), ws_obj("ws-1", "karthik"))]).await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/workspaces/ws-1/push", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
}
```

with the helpers updated to the new schema:

```rust
fn ws_obj(name: &str, owner: &str) -> Value {
    json!({
        "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "Workspace",
        "metadata": {"name": name, "labels": {"kloudlite-git.io/owner": owner}},
        "spec": {
            "owner": owner, "team": "", "name": name, "region": "centralindia", "image": "nginx:alpine",
            "storage": {"quotaGb": 20}, "desiredState": "running"
        }
    })
}
```

and, in `api_volumes.rs`, replace the registry-stub history tests with SnapshotRequest-backed ones:

```rust
fn snap_obj(name: &str, volume: &str, id: &str, at: &str, message: Option<&str>) -> Value {
    let mut v = json!({
        "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "SnapshotRequest",
        "metadata": {"name": name, "labels": {"kloudlite-git.io/owner": "karthik", "kloudlite-git.io/volume": volume}},
        "spec": {"volume": volume},
        "status": {"phase": "done", "snapshotId": id, "at": at}
    });
    if let Some(m) = message {
        v["spec"]["message"] = json!(m);
    }
    v
}

/// The wire shape the web reads has not moved — only where it comes from. `id`, `created_at` and
/// `message` are what the snapshots page renders, and `/history` still answers newest first.
#[tokio::test]
async fn history_lists_done_snapshot_requests_newest_first() {
    let list = json!({
        "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "SnapshotRequestList", "metadata": {},
        "items": [
            snap_obj("snap-a", "ws-1", "c1", "2026-08-27T09:00:00Z", Some("first")),
            snap_obj("snap-b", "ws-1", "c2", "2026-08-27T10:00:00Z", None),
            // A request still running is a wish, not a snapshot; it must not appear.
            json!({"apiVersion": "kloudlite-git.io/v1alpha1", "kind": "SnapshotRequest",
                   "metadata": {"name": "snap-c", "labels": {"kloudlite-git.io/volume": "ws-1"}},
                   "spec": {"volume": "ws-1"}, "status": {"phase": "working"}})
        ]
    });
    let s = server(None, vec![
        kget(format!("{API}/workspaces/ws-1"), ws_obj("ws-1", "karthik")),
        kget(format!("{API}/snapshotrequests"), list),
    ])
    .await;
    let tok = token(&s.jwt, "karthik");

    let resp = reqwest::Client::new()
        .get(format!("{}/v1/volumes/ws-1/history", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let records: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(records.len(), 2, "only `done` requests are snapshots: {records:?}");
    assert_eq!(records[0]["id"], "c2");
    assert_eq!(records[1]["id"], "c1");
    assert_eq!(records[1]["message"], "first");
    assert!(records[0]["created_at"].is_string(), "the web reads created_at: {}", records[0]);
}

#[tokio::test]
async fn refs_reports_the_newest_done_snapshot_as_main() {
    let list = json!({
        "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "SnapshotRequestList", "metadata": {},
        "items": [snap_obj("snap-b", "ws-1", "c2", "2026-08-27T10:00:00Z", None)]
    });
    let s = server(None, vec![
        kget(format!("{API}/workspaces/ws-1"), ws_obj("ws-1", "karthik")),
        kget(format!("{API}/snapshotrequests"), list),
    ])
    .await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new()
        .get(format!("{}/v1/volumes/ws-1/refs", s.base))
        .bearer_auth(&tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<Value>().await.unwrap()["main"], "c2");
}
```

Delete `volume_history_without_a_configured_registry_is_503` — with the registry read gone from
`bins/api` there is no unconfigured state for these two routes to be in.

And one more, for the pointer that replaces `Volume.status.lastPush` (D5/D5a):

```rust
/// "Has this ever been pushed" is a query over `done` SnapshotRequests, not a field on the Volume.
/// A second controller writing the Volume's status would have its field pruned by the Volume
/// reconciler's next force-apply, so the answer lives where the writer is.
#[tokio::test]
async fn only_a_volume_with_a_done_snapshot_reports_a_registry_pointer() {
    let snaps = json!({
        "apiVersion": "kloudlite-git.io/v1alpha1", "kind": "SnapshotRequestList", "metadata": {},
        "items": [snap_obj("snap-a", "ws-1", "c1", "2026-08-27T09:00:00Z", None)]
    });
    let s = server(
        None,
        vec![
            kget(format!("{API}/volumes"), vol_list(vec![
                vol_obj("ws-1", "karthik", "workspace"),
                vol_obj("env-1", "karthik", "environment"),
            ])),
            kget(format!("{API}/snapshotrequests"), snaps),
        ],
    )
    .await;
    let tok = token(&s.jwt, "karthik");
    let resp = reqwest::Client::new().get(format!("{}/v1/volumes", s.base)).bearer_auth(&tok).send().await.unwrap();
    let list: Vec<Value> = resp.json().await.unwrap();
    let ws = list.iter().find(|v| v["name"] == "ws-1").unwrap();
    assert_eq!(ws["volume"], "vol/karthik/ws-1");
    let env = list.iter().find(|v| v["name"] == "env-1").unwrap();
    assert!(env["volume"].is_null(), "never pushed means no registry pointer yet");
    // ONE list, not one per row.
    assert_eq!(
        s.rec.calls().iter().filter(|c| c.contains("snapshotrequests")).count(),
        1,
        "the pushed-set is one label list: {:?}", s.rec.calls()
    );
}
```

`vol_obj` loses its `pushed` parameter in this task (there is no `status.lastPush` to set any
more): `fn vol_obj(name: &str, owner: &str, kind: &str) -> Value`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p kloudlite-git-workspaces`
Expected: FAIL — `api.rs` still calls `place_node`/`create_volume` and reads `spec.volume_ref`.

- [ ] **Step 3: Write the implementation**

In `crates/workspaces/src/api.rs`:

- Delete `place_node` (381-389), `create_volume` (391-400), `workspace_for` (443-470) and
  `environment_for` (785-808) — the last two collapse into their callers.
- `ws_doc`/`env_doc` (340-371) read placement and the volume pointer from STATUS:

```rust
/// `pushed` is the set of volume names that have at least one `done` SnapshotRequest — ONE label
/// list per request, built by `pushed_volumes` below, not one lookup per row.
fn ws_doc(w: &crd::Workspace, pushed: &HashSet<String>) -> Workspace {
    let id = w.name_any();
    let st = w.status.as_ref();
    Workspace {
        owner: w.spec.owner.clone(),
        team: w.spec.team.clone(),
        name: w.spec.name.clone(),
        region: w.spec.region.clone(),
        state: phase(st.map(|s| s.phase.as_str()), WsState::Creating),
        image: w.spec.image.clone(),
        // `None` until a node claims it — the web renders that as "not placed yet" rather than as
        // a node that was never true.
        placement: st.map(|s| s.node_name.clone()).filter(|n| !n.is_empty()),
        volume: pushed.contains(&id).then(|| format!("vol/{}/{id}", w.spec.owner)),
        quota_gb: w.spec.storage.quota_gb,
        live_state: serde_json::Value::Null,
        id,
    }
}

/// Every volume of `owner` that has ever landed a snapshot.
///
/// This replaces `Volume.status.lastPush` and it is a QUERY, not a field, because a field would
/// need a second controller writing the Volume's status — and `patch_status` force-applies under
/// one field manager, so the Volume reconciler's next pass would prune it (server-side apply
/// removes fields a manager previously owned and no longer sets).
async fn pushed_volumes(c: &kube::Client, owner: &str) -> Result<HashSet<String>, Response> {
    let api: Api<crd::SnapshotRequest> = Api::all(c.clone());
    let lp = ListParams::default().labels(&format!("{OWNER_LABEL}={owner}"));
    Ok(api
        .list(&lp)
        .await
        .map_err(kube_err)?
        .items
        .into_iter()
        .filter(|r| r.status.as_ref().is_some_and(|s| s.phase == crd::Phase::Done))
        .map(|r| r.spec.volume)
        .collect())
}
```

  with `env_doc` mirroring it. Delete `volume_ptr` (334-338) entirely; every caller takes the
  `pushed` set instead. `list_ws`, `get_ws`, `list_env`, `get_env` and `list_volumes` each make one
  `pushed_volumes` call and pass the set down — `volumes_of` stays for `quota_gb` on the env side.
- `create_ws` (472-523) becomes: validate team, validate `repo`/`branch` exactly as today
  (lines 487-509 minus `credential_secret`), then one create:

```rust
    let mut w = crd::Workspace::new(
        &id,
        crd::WorkspaceSpec {
            owner: owner.clone(),
            team: team.clone(),
            name: body.name,
            region: body.region,
            image: body.image,
            storage: crd::WorkspaceStorage { quota_gb: body.quota_gb, source },
            desired_state: DesiredState::Running,
            resources: Default::default(),
        },
    );
    let mut l = labels(&owner, "workspace");
    l.insert(TEAM_LABEL.to_string(), team.clone());
    w.metadata.labels = Some(l);
    let api: Api<crd::Workspace> = Api::all(c.clone());
    let w = api.create(&PostParams::default(), &w).await.map_err(kube_err)?;
    install_user_key_after_placed(&s, c, &owner, &team, &id).await;
    Ok((StatusCode::ACCEPTED, Json(ws_doc(&w, None))).into_response())
```

- `install_user_key` (533-556) gains the poll the spec asks for:

```rust
/// Put the owner's platform key in their workspace namespace, once the claim has created it.
///
/// The namespace is the CONTROLLER's to make, so on a first workspace it does not exist at the
/// moment of the create. Waiting for `Placed` — not for the namespace — is the cheapest signal that
/// a node has taken the object and its OwnerBinding reconciler is running.
///
/// Best effort with a 5 s ceiling: a key that lands on the next list costs the workspace its git
/// identity for a moment, not its existence. `list_ws` retries the install when the Secret is
/// absent, which is what closes the first-workspace-without-a-key gap for good.
async fn install_user_key_after_placed(s: &ApiState, c: &kube::Client, owner: &str, team: &str, id: &str) {
    let api: Api<crd::Workspace> = Api::all(c.clone());
    for _ in 0..10 {
        match api.get_opt(id).await {
            Ok(Some(w))
                if w.status.is_some_and(|st| {
                    st.conditions.iter().any(|cd| cd.type_ == "Placed" && cd.status == "True")
                }) =>
            {
                install_user_key(s, c, owner, team).await;
                return;
            }
            _ => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
        }
    }
    tracing::info!(%owner, workspace = %id, "not placed within 5s; the key install is left to the next list");
}
```

  `install_user_key` itself is unchanged (it already writes the Secret with the per-namespace grant).
- `list_ws` (558-579) joins volumes by the workspace's own name (`D1`), and retries the key install
  when the namespace's Secret is missing:

```rust
    let list: Vec<_> = items.iter().map(|w| ws_doc(w, vols.get(&w.name_any()))).collect();
    // The retry the create's 5s ceiling defers to: cheap, idempotent, and the only place a user
    // whose very first workspace outran its namespace is ever seen again.
    if !items.is_empty() {
        let secrets: Api<k8s_openapi::api::core::v1::Secret> =
            Api::namespaced(c.clone(), &crd::ws_namespace(&owner, &team));
        if matches!(secrets.get_opt(crate::k8s::USER_KEY_SECRET).await, Ok(None)) {
            install_user_key(&s, c, &owner, &team).await;
        }
    }
```

- `get_ws` (592-602), `list_env`, `get_env` read the Volume by `w.name_any()`/`e.name_any()`.
- `delete_ws` (610-625) and `delete_env` (914-929) drop their second `vol.delete(...)` call.
- `clone_ws` (657-682) and `restore_ws` (693-727) build a `Workspace` with the matching
  `storage.source` and no node. `restore_ws`'s registry lookup becomes a CR lookup:

```rust
    // A restore grafts onto an explicit PUSHED commit. The record is named by a `done`
    // SnapshotRequest now; no registry read on the request path.
    let snaps: Api<crd::SnapshotRequest> = Api::all(c.clone());
    let lp = ListParams::default().labels(&format!("{}={}", crd::VOLUME_LABEL, body.src_workspace));
    let found = snaps.list(&lp).await.map_err(kube_err)?.items.into_iter().any(|r| {
        r.status.is_some_and(|st| st.phase == "done" && st.snapshot_id.as_deref() == Some(body.snapshot_id.as_str()))
    });
    if !found {
        return Err(not_found());
    }
```

- `request_push`/`push_ws`/`push_env` (958-1014) become one helper:

```rust
/// A push is an object, not an annotation: a wish WITH AN OUTCOME needs somewhere to put the
/// outcome.
///
/// The spec is `{volume, message?}` and nothing else. A node is a controller-owned fact: copying it
/// here would go stale the moment node retirement moves the Volume, and it would put the API in the
/// business of authoring spec from facts it does not own. The agent resolves the node from the
/// named Volume — every agent watches every request and acts only on its own.
///
/// The Volume still has to EXIST, though: a push against a workspace whose disk has not been made
/// yet is a 409 the user can act on, not a request that sits pending forever.
async fn request_snapshot(
    c: &kube::Client,
    owner: &str,
    volume: Option<&str>,
    message: Option<String>,
) -> Result<Response, Response> {
    let Some(volume) = volume else {
        return Err((StatusCode::CONFLICT, "not ready yet: no volume for this workspace").into_response());
    };
    let vols: Api<crd::Volume> = Api::all(c.clone());
    if vols.get_opt(volume).await.map_err(kube_err)?.is_none() {
        return Err((StatusCode::CONFLICT, "not ready yet: no volume for this workspace").into_response());
    }
    let name = rid("snap");
    let mut r = crd::SnapshotRequest::new(
        &name,
        crd::SnapshotRequestSpec {
            volume: volume.to_string(),
            message,
        },
    );
    r.metadata.labels = Some(BTreeMap::from([
        (OWNER_LABEL.to_string(), owner.to_string()),
        (crd::VOLUME_LABEL.to_string(), volume.to_string()),
    ]));
    let api: Api<crd::SnapshotRequest> = Api::all(c.clone());
    let created = api.create(&PostParams::default(), &r).await.map_err(kube_err)?;
    Ok((StatusCode::ACCEPTED, Json(snapshot_doc(&created))).into_response())
}
```

  with `push_ws` passing `w.status.as_ref().and_then(|s| s.volume_ref.as_deref())`.
- `volume_history`/`volume_refs` (1082-1117) list SnapshotRequests and project them into the
  `CommitRecord` shape (D2/D3):

```rust
/// The snapshots of one volume, newest first — the same wire shape `/history` has always answered,
/// now read from CRs instead of the registry. The bytes still live on the server tier; what moved
/// is where the INDEX of them lives, so a history page no longer depends on a cross-tier call.
async fn done_snapshots(c: &kube::Client, volume: &str, region: &str) -> Result<Vec<crate::registry::CommitRecord>, Response> {
    let api: Api<crd::SnapshotRequest> = Api::all(c.clone());
    let lp = ListParams::default().labels(&format!("{}={volume}", crd::VOLUME_LABEL));
    let mut out: Vec<crate::registry::CommitRecord> = api
        .list(&lp)
        .await
        .map_err(kube_err)?
        .items
        .into_iter()
        .filter_map(|r| {
            let st = r.status?;
            if st.phase != "done" {
                return None;
            }
            Some(crate::registry::CommitRecord {
                id: st.snapshot_id?,
                state: serde_json::Value::Null,
                // The lineage lives in the record on the server tier; nothing that reads this
                // projection uses it, and copying it into etcd would put megabytes of layer
                // bookkeeping into an object the API server lists.
                lineage: vec![],
                region: region.to_string(),
                message: r.spec.message,
                created_at: st.at.and_then(|a| chrono::DateTime::parse_from_rfc3339(&a).ok())
                    .map(|d| d.with_timezone(&chrono::Utc))
                    .unwrap_or_else(chrono::Utc::now),
            })
        })
        .collect();
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(out)
}
```

  `volume_refs` answers `{"main": done_snapshots(..).first().map(|r| r.id.clone())}`.
- Delete `ApiState::registry`, `with_registry`, the `registry()` helper (1056-1060) and the
  `RegistryClient`/`MAIN_REF` import (line 21); keep `MAIN_REF`'s value by inlining `"main"` at the
  one remaining use, or import it from `registry_client` without the client. Remove the
  `with_registry` wiring at `bins/api/src/main.rs:120-131` and the warning beside it.

RBAC and DaemonSet:

- `deploy/k3s/agent-rbac.yaml`: replace the three CRD rules (lines 26-38) with:

```yaml
  # The work items. `status` is a separate subresource with a separate grant: the controller writes
  # status and must NOT be able to rewrite spec, which is the desired state the API owns. That
  # split is the whole reason the CRDs declare a status subresource.
  #
  # `create` on volumes, snapshotrequests and ownerbindings is new, and it is the point of this
  # change: a controller that creates a child owns it. The API lost the same three verbs.
  # ponytail: `patch`/`update` on the parents' main resource is wider than `heal_labels` needs — a
  # ValidatingAdmissionPolicy refusing a non-label main-resource patch from this SA is the
  # mechanical version.
  - apiGroups: ["kloudlite-git.io"]
    resources: ["workspaces", "environments"]
    verbs: ["get", "list", "watch", "patch", "update"]
  - apiGroups: ["kloudlite-git.io"]
    resources: ["volumes", "snapshotrequests", "ownerbindings"]
    verbs: ["get", "list", "watch", "create", "patch", "update"]
  - apiGroups: ["kloudlite-git.io"]
    resources:
      ["volumes/status", "workspaces/status", "environments/status",
       "snapshotrequests/status", "ownerbindings/status"]
    verbs: ["get", "patch", "update"]
  # Finalizers are edited on the main resource, but Kubernetes gates them behind their own verb.
  # `snapshotrequests/finalizers` is required: a delete during an in-flight push must wait, or it
  # orphans a btrfs send, a stage file and a blob upload with no object left to record the outcome.
  - apiGroups: ["kloudlite-git.io"]
    resources:
      ["volumes/finalizers", "workspaces/finalizers", "environments/finalizers",
       "snapshotrequests/finalizers"]
    verbs: ["update"]
```

  Note the agent gets no `delete` on `snapshotrequests` — it never deletes one; the Environment's
  stop child goes by garbage collection through its ownerReference.

- `deploy/k3s/api-rbac.yaml`: replace the CRD rule (lines 16-19) and delete the `nodes`/`pods` rule
  (lines 21-23 — placement left the API entirely):

```yaml
  # Spec is the API's to write. It never writes status — that is the controller's, and the split is
  # what stops the two overwriting each other's view of the same object.
  - apiGroups: ["kloudlite-git.io"]
    resources: ["workspaces", "environments"]
    verbs: ["get", "list", "watch", "create", "patch", "update", "delete"]
  # A push. `create` and `delete` but deliberately NO `/status`: the outcome of a push is the
  # agent's to write, and an API that could write it could report a snapshot that never happened.
  - apiGroups: ["kloudlite-git.io"]
    resources: ["snapshotrequests"]
    verbs: ["get", "list", "create", "delete"]
  # Read-only, for projections. `create` went to the agent (a controller owns the children it
  # authors) and `delete` went with it — the API's delete handler is now one call to the Workspace,
  # and garbage collection follows the ownerReference to the Volume.
  - apiGroups: ["kloudlite-git.io"]
    resources: ["volumes"]
    verbs: ["get", "list"]
```

  `ownerbindings` disappears from this file entirely: the claiming agent creates them now.
- `deploy/k3s/agent-daemonset.yaml:69-71`: delete `WS_GIT_BASE`, add:

```yaml
            # Where a `gitRepo` workspace clones from — over SSH, from inside the pod, as the
            # owner. `repo` is `owner/name`, never a URL, so this is the only thing that decides
            # which host is reachable.
            - name: WS_GIT_SSH_HOST
              value: cr.khost.dev
            - name: WS_GIT_SSH_PORT
              value: "22"
            # Pinned so seeding works with any workspace image, including ones with no git.
            - name: WS_GIT_INIT_IMAGE
              value: alpine/git:2.45.2
            - name: WS_REGION
              value: centralindia
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p kloudlite-git-workspaces` then `cargo test --workspace`
Expected: PASS.

- [ ] **Step 5: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: no warnings (CI gates on exactly this).

- [ ] **Step 6: Commit**

```bash
git add crates/workspaces/src/api.rs crates/workspaces/tests bins/api/src/main.rs deploy/k3s
git commit -m "Write one object per user action in the workspaces API"
```

---

## Task 9: The snapshots page stops promising layer detail

**Files:**
- Modify: `web/apps/web/src/app/(shell)/[owner]/(org)/snapshots/[id]/page.tsx:55-77`
- Modify: `web/apps/web/src/lib/api.ts:766-776` (`ApiLineageEntry`, `ApiCommitRecord.lineage`)
- Verify unchanged: `web/apps/web/src/components/app/{workspace-list,volume-list,restore-dialog}.tsx`,
  `.../workspaces/{page.tsx,actions.ts}`, `.../snapshots/page.tsx`

**Interfaces:**
- Consumes: `/v1` as Task 8 left it — `ApiWorkspace { id, owner, team, name, region, state, image,
  placement, volume, quota_gb, live_state }` (unchanged), `ApiVolumeSummary { name, kind, volume }`
  (unchanged), history as `[{id, state, lineage, region, message?, created_at}]` with `lineage`
  now always `[]` (D2), refs as `{"main": string | null}` (no web caller).
- Produces: nothing new.

**What was checked, so the implementer does not re-check it.** Grepped across `web/apps/web/src`:

- `ApiWorkspace.placement`, `.volume` and `.live_state` are **declared and never read** anywhere in
  the web tier. `placement` is already typed `string | null` (`lib/api.ts:644`). The API answering
  `null` between create and claim is therefore invisible — no change needed.
- `workspace-list.tsx:176-192` reads only `id`, `name`, `state`, `region`, `quota_gb`, `image`, and
  chooses start-vs-stop on `state === "stopped"`. `quota_gb` still comes from
  `spec.storage.quotaGb` after Task 8's `ws_doc`, so it is unchanged.
- `volume-list.tsx:53-70` reads `name`, `kind`, `volume`; `!v.volume` renders "Not pushed yet",
  which Task 8 keeps working via `pushed_volumes` — a label list of `done` SnapshotRequests
  (D5/D5a). The field name the web reads (`volume`) does not change.
- `restore-dialog.tsx` reads no API fields at all — three hidden inputs from its parent.
- `CommitRecord.region` and `.state` are never rendered.
- `/v1/volumes/{name}/refs` has **no web caller** at all; nothing to adapt there.

The one real break is `lineage`, in exactly one file. That is this task.

- [ ] **Step 1: Cut the layers/sha fragment from the snapshot row**

Replace `snapshots/[id]/page.tsx:55-77` with:

```tsx
            {history.value.map((c) => (
              <li key={c.id} className="flex flex-wrap items-center gap-4 px-5 py-4">
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-3">
                    <span className="font-mono text-sm2">{c.id.slice(0, 8)}</span>
                    <span className={`truncate text-sm2 ${c.message ? "text-foreground" : "text-muted-foreground/50 italic"}`}>
                      {c.message || "—"}
                    </span>
                  </div>
                  {/* Layer counts and the tip sha are gone with the registry read: a snapshot's
                      lineage is layer bookkeeping that lives with the bytes on the server tier, and
                      copying it into a CR would put megabytes into an object the API server lists.
                      What a person picks a snapshot by — when, and what they called it — is here. */}
                  <span className="mt-1 block text-caption text-muted-foreground">
                    {when(new Date(c.created_at).getTime())}
                  </span>
                </div>
                {kind === "workspace" && <RestoreDialog owner={owner} srcWorkspace={id} snapshotId={c.id} />}
              </li>
            ))}
```

Keep the surrounding `<ul>`/empty-state exactly as it is — copy existing siblings, not new patterns
(`web/apps/web/CLAUDE.md`).

- [ ] **Step 2: Drop the now-unreachable type**

In `lib/api.ts`, delete `ApiLineageEntry` (line 766) and change `ApiCommitRecord`'s field to:

```ts
  /** Always empty: the lineage lives with the bytes on the server tier, not in the CR the
   *  `/history` projection now reads. Kept on the wire so an older client still parses. */
  lineage: never[];
```

- [ ] **Step 3: Typecheck, lint and test**

```bash
cd web && bun run typecheck && bun run lint && bun run test
```
Expected: PASS. `bunx tsc --noEmit -p apps/web/tsconfig.json` is the authority if the editor
disagrees — its diagnostics here are frequently stale.

- [ ] **Step 4: Commit**

```bash
git add web/apps/web/src
git commit -m "Show a snapshot by when and what it says, not by its layers"
```

---

## Task 10: End-to-end phase and the deploy note

**Files:**
- Modify: `tests/ws_e2e.sh` — new phase inserted between line 457 (end of the restore phase) and
  line 459 (the environment header); new teardown var registered at lines 96-101 and added to the
  loop at 109-111; the final `OK:` string at line 563 extended
- Modify: `deploy/k3s/agent-daemonset.yaml` (top comment block) — the roll order
- Modify: `deploy/k3s/README.md` — "Release 1: controller ownership", the runbook with the exact
  commands (the yaml comment states the ORDER; the README is what an operator follows)
- Modify: `README.md` and `CLAUDE.md` — the workspaces prose, which still described API-side
  placement, job kinds and `credential_secret`

**As built, the script needed a git phase before the seeded one** (there was none): the run pushes
a two-commit repository with the real `git` binary to the server tier it already starts, over
`admin create-repo`/`admin add-token`, and installs a platform key with `admin add-key` plus the
private key at `auth/userkey/{owner}` — exactly the two objects `/v1/platform-key` writes, which
cannot be called here because it needs the Mongo directory this script does not run. That forced
two smaller changes: the server and the api now share ONE `file://` store rather than a per-process
`mem://`, and the ssh listener binds every interface, because the seeding init container clones
from inside a pod over the node's InternalIP (`WS_GIT_SSH_HOST`/`WS_GIT_SSH_PORT` on the agent).

**Interfaces:**
- Consumes: everything above.
- Produces: a phase that proves git seeding works on a FIRST workspace, which is the user-visible
  bug this whole design was written for.

**Read the script's own idioms before writing this** — they are not the generic ones:
`$BASE` (not `$API`), `$USER_TOKEN` (not `$TOKEN`), `$REGION_ID`, `fail "…"` (not `exit 1`),
`log "…"` (not `echo "== N. …"`), `field id` (not `jq -r .id`), `id_count` for history length,
`wait_ws_ready`/`wait_ws_gone` (which are `kubectl wait --for=condition=Ready` /
`--for=delete`), `live_dir`, and `quota_gb` — snake_case — in the create body.

- [ ] **Step 1: Register the new object for teardown**

At lines 96-101, beside `WS_ID`/`CLONE1_ID`/`CLONE_ID`/`RESTORE_ID`/`ENV_ID`, add `SEED_ID=""`,
and add it to the delete loop at 109-111 exactly as its siblings appear there. Without this a
failed run leaves a workspace behind and the next run's assertions drift.

- [ ] **Step 2: Insert the phase between lines 457 and 459**

```sh
log "creating a workspace seeded from a platform repository"
# The 27 Aug bug, as a test. "Open in a workspace" produced a pod stuck on `path … does not exist`
# forever: the workspace made its pod before its Volume had made the disk, and the git-seeding path
# was never wired end to end anyway — the API named a token Secret nobody wrote and the agent had
# no permission to read one. Both halves are asserted here.
SEED_JSON=$(curl -fsS -X POST "$BASE/v1/workspaces" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"name":"e2e-seeded","region":"'"$REGION_ID"'","quota_gb":5,"repo":"'"$USER_NAME"'/'"$E2E_REPO"'","branch":"main"}')
SEED_ID=$(echo "$SEED_JSON" | field id)
[ -n "$SEED_ID" ] || fail "no id in seeded workspace create response: $SEED_JSON"

log "checking the API wrote ONE object and named no node"
[ -z "$(kubectl get workspace "$SEED_ID" -o jsonpath='{.spec.nodeName}' 2>/dev/null)" ] \
  || fail "the API named a node; placement is a fact the controllers establish"
[ -z "$(kubectl get workspace "$SEED_ID" -o jsonpath='{.spec.volumeRef}' 2>/dev/null)" ] \
  || fail "the API named a child; volumeRef in spec was a wish about a fact"

log "waiting for the claim, then for the workspace"
kubectl wait --for=condition=Placed "workspace/$SEED_ID" --timeout=120s \
  || fail "workspace $SEED_ID was never claimed by any node"
wait_ws_ready "$SEED_ID"

log "checking the Volume is a child that dies with its parent"
kubectl get volume "$SEED_ID" -o jsonpath='{.metadata.ownerReferences[0].kind}' | grep -qx Workspace \
  || fail "the Volume has no controlling Workspace ownerReference"
[ "$(kubectl get workspace "$SEED_ID" -o jsonpath='{.status.volumeRef}')" = "$SEED_ID" ] \
  || fail "status.volumeRef does not report the child"

log "checking the init container actually cloned the repository into the workspace"
kubectl -n "$WS_NS" exec "$SEED_ID" -c workspace -- sh -c 'ls -a /workspace/.git >/dev/null' \
  || fail "no .git in /workspace: the git-seeding init container did not run or did not clone"

log "pushing the seeded workspace and reading its history back from SnapshotRequests"
SEED_BEFORE=$(id_count "$(curl -fsS "$BASE/v1/volumes/$SEED_ID/history" -H "Authorization: Bearer $USER_TOKEN")")
curl -fsS -X POST "$BASE/v1/workspaces/$SEED_ID/push" -H "Authorization: Bearer $USER_TOKEN" \
  -H 'Content-Type: application/json' -d '{"message":"seeded push"}' >/dev/null
SEED_AFTER="$SEED_BEFORE"
SEED_HISTORY=""
for i in $(seq 1 30); do
  SEED_HISTORY=$(curl -fsS "$BASE/v1/volumes/$SEED_ID/history" -H "Authorization: Bearer $USER_TOKEN")
  SEED_AFTER=$(id_count "$SEED_HISTORY")
  [ "$SEED_AFTER" -gt "$SEED_BEFORE" ] && break
  sleep 2
done
[ "$SEED_AFTER" -eq "$((SEED_BEFORE + 1))" ] \
  || fail "history did not grow by exactly one after push ($SEED_BEFORE -> $SEED_AFTER)"
echo "$SEED_HISTORY" | grep -q '"message":"seeded push"' || fail "push message missing: $SEED_HISTORY"
echo "$SEED_HISTORY" | grep -q '"created_at":"' || fail "history lost the created_at the web reads"
kubectl get snapshotrequests -l "kloudlite-git.io/volume=$SEED_ID" -o name | grep -q . \
  || fail "a push wrote no SnapshotRequest"

log "deleting the seeded workspace with ONE call and letting GC take the child"
curl -fsS -X DELETE "$BASE/v1/workspaces/$SEED_ID" -H "Authorization: Bearer $USER_TOKEN" >/dev/null
wait_ws_gone "$SEED_ID"
kubectl wait --for=delete "volume/$SEED_ID" --timeout=300s \
  || fail "the Volume outlived its Workspace: garbage collection did not follow the ownerReference"
SEED_ID=""
```

`$E2E_REPO` is the repository the run's earlier git phase pushes; read the variable that phase
actually sets out of the script and use that name — if the run has no git phase yet, add one that
`git push`es a two-commit repo to `$USER_NAME/e2e-seed` over the server tier's SSH listener before
this block, since a seeded workspace needs something to clone.

- [ ] **Step 3: Extend the final `OK:` line (563)**

Add `git-seeded workspace` to the enumeration, in the same style as the phases already listed.

- [ ] **Step 4: Run it, knowing it will skip locally**

Run: `./tests/ws_e2e.sh; echo "exit=$?"`
Expected on this Mac: `exit=77` — a prerequisite (root-capable btrfs, a reachable k3s cluster with
the CRDs installed, `COSMOS_*`/`AZURE_*`) is missing. **77 is a skip, not a pass.** The phase is
verified in CI or on the Linux btrfs VM; record which one ran it in the task report.

- [ ] **Step 5: Write the deploy note**

Add to the top comment block of `deploy/k3s/agent-daemonset.yaml`:

```yaml
# Roll order for the controller-ownership change (2026-08-27), and it is not the usual one:
#
#   1. kubectl delete workspace ws-16980a570dd6eecd   # the stuck one; it predates status.nodeName
#   2. kubectl apply -f deploy/k3s/crds.yaml          # BEFORE the agent, or the new watches 404
#   3. kubectl apply -f deploy/k3s/{agent,api}-rbac.yaml
#   4. kubectl apply -f deploy/k3s/agent-daemonset.yaml
#
# The CRDs first because the agent's placement watch selects on `.status.nodeName`, which does not
# exist as a selectable field until the CRD carries it — a watch on an undeclared selectable field
# is refused, and the agent would come up converging nothing while reporting healthy. Removing
# `.spec.nodeName` from `selectableFields` while a client still uses that selector answers 400, not
# an empty list, which is the other half of why the agent and the CRDs move together. The agent's
# startup migration adopts existing Volumes and backfills history on its first boot after step 4.
#
# This is RELEASE 1: the CRD still carries `spec.nodeName`/`spec.volumeRef` as optional fields.
# Release 2 (Task 11) drops them, and only after every node has rolled — see that task's gate.
# Release 1 can be rolled back (old agents ignore the new status fields); release 2 cannot.
```

Steps 2 and 4 are ONE operation, not two changes with a soak between them (ledger ruling): the old
agent's Workspace/Environment watch 4xx's for that window, so it is kept to the length of a second
`kubectl apply`. Both RBAC yamls are already applied on dev, as is `crds.yaml`.

The operator-facing runbook goes in `deploy/k3s/README.md` under "Release 1: controller ownership",
with the exact commands — `KUBECONFIG=.local/k3s.yaml` for the k3s side, the default context for
the AKS api tier — and two steps the yaml comment does not carry: watching the `migration:` lines
and `kubectl get workspaces -o custom-columns=…` until `status.nodeName` is populated, then rolling
`deploy/kloudlite-git.yaml` and verifying a fresh "Open in a workspace" clone. The image pins in both
yamls are edited after the push, once CI has built the SHAs.

- [ ] **Step 6: Commit**

```bash
git add tests/ws_e2e.sh deploy/k3s/agent-daemonset.yaml deploy/k3s/README.md README.md CLAUDE.md
git commit -m "Prove git seeding end to end and pin the roll order"
```

---

## Task 11: Release 2 — drop the legacy spec fields

**Do not start this task in the same session as Tasks 1-10.** It is a separate release, gated on
evidence from a cluster that has been running Release 1. A reviewer approving Task 10 is not
approving this.

**Files:**
- Modify: `crates/workspaces/src/crd.rs` — remove `node_name` and `volume_ref` from `WorkspaceSpec`
  and `EnvironmentSpec`
- Modify: `deploy/k3s/crds.yaml` (regenerated)
- Modify: `crates/workspaces/tests/crd_yaml.rs` —
  `release_one_adds_storage_and_keeps_the_legacy_spec_fields` becomes its inverse
- Modify: `bins/agent/src/migrate.rs` — the fallback read of the parent's own `spec.nodeName` goes

**Interfaces:**
- Consumes: `crd::WorkspaceSpec`/`EnvironmentSpec` as Task 1 left them.
- Produces: the same types minus two fields. Nothing else changes shape.

**Why this is its own task and its own release.** Pruning is irreversible and cluster-wide: the
moment the field leaves the schema, the API server strips it from every stored object on that
object's next write, everywhere, at once. The agent roll is per node. Doing both in one release
loses the Volume pointer of every object whose node had not rolled yet, and there is no way back —
the old agent cannot read a field that no longer exists in storage.

- [ ] **Step 1: Gate — prove nothing needs the fields**

Run against the production cluster, after every node has been on Release 1 long enough to have
reconciled everything (all three must hold):

```sh
# (a) Every Workspace and Environment reports its child in status.
kubectl get workspaces,environments \
  -o jsonpath='{range .items[*]}{.kind}/{.metadata.name} volumeRef={.status.volumeRef} node={.status.nodeName}{"\n"}{end}'
# Expect: no row with an empty volumeRef or an empty node.

# (b) Nothing still carries the legacy spec fields.
kubectl get workspaces,environments \
  -o jsonpath='{range .items[*]}{.metadata.name} {.spec.volumeRef} {.spec.nodeName}{"\n"}{end}' | grep -v '^\S* *$' || echo CLEAN
# Expect: CLEAN.

# (c) Every node is running the Release 1 image.
kubectl -n kube-system get pods -l app=kloudlite-git-agent \
  -o jsonpath='{range .items[*]}{.spec.nodeName} {.spec.containers[0].image}{"\n"}{end}'
# Expect: every btrfs node listed, all on the same Release 1 SHA.
```

Paste all three outputs into the task report. **If any check fails, STOP** — the fields stay and
this task waits. This is not a formality: (b) failing means an object would lose data.

- [ ] **Step 2: Flip the test**

Replace `release_one_adds_storage_and_keeps_the_legacy_spec_fields` in
`crates/workspaces/tests/crd_yaml.rs` with:

```rust
/// Release 2. The two legacy spec fields are gone from the schema, which means the API server
/// prunes them on read — safe now, and only now, because every object carries `status.volumeRef`
/// and every node has rolled (see the gate in the plan's Task 11 Step 1).
///
/// There is no rollback past this point: a field pruned from storage cannot be read back by an
/// older agent.
#[test]
fn release_two_prunes_the_legacy_spec_fields() {
    use kube::CustomResourceExt;
    use kloudlite_git_workspaces::crd::{Environment, Workspace};
    for crd in [Workspace::crd(), Environment::crd()] {
        let props = crd.spec.versions[0]
            .schema
            .as_ref()
            .unwrap()
            .open_api_v3_schema
            .as_ref()
            .unwrap()
            .properties
            .as_ref()
            .unwrap()["spec"]
            .properties
            .as_ref()
            .unwrap()
            .clone();
        assert!(props.contains_key("storage"), "{} spec needs storage", crd.spec.names.kind);
        assert!(!props.contains_key("nodeName"), "{} spec still has nodeName", crd.spec.names.kind);
        assert!(!props.contains_key("volumeRef"), "{} spec still has volumeRef", crd.spec.names.kind);
    }
}
```

- [ ] **Step 3: Run it to verify it fails**

Run: `cargo test -p kloudlite-git-workspaces --test crd_yaml release_two`
Expected: FAIL — "Workspace spec still has nodeName".

- [ ] **Step 4: Remove the fields**

In `crates/workspaces/src/crd.rs`, delete from `WorkspaceSpec` and `EnvironmentSpec`:

```rust
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_ref: Option<String>,
```

In `bins/agent/src/migrate.rs`, delete the fallback that reads the parent's own `spec.nodeName` —
after this release there is no such field, and the Volume's spec was always the better source
anyway. `migrate::once` itself stays: it is idempotent, it runs on every restart, and a cluster
restored from a backup can still present an orphan Volume.

- [ ] **Step 5: Regenerate and run**

Run: `CRD_REGEN=1 cargo test -p kloudlite-git-workspaces --test crd_yaml`
Then: `cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/workspaces/src/crd.rs crates/workspaces/tests/crd_yaml.rs \
        deploy/k3s/crds.yaml bins/agent/src/migrate.rs
git commit -m "Drop the migrated workspace spec fields"
```

- [ ] **Step 7: Apply, in this order**

```sh
kubectl apply -f deploy/k3s/crds.yaml
```

No agent roll is needed — the agent stopped reading those fields in Release 1. Re-run the gate's
check (a) afterwards to confirm nothing lost its `status.volumeRef`.

---

## Self-Review

**1. Spec coverage.**

| Spec section | Task |
|---|---|
| Objects: `Workspace.storage`, status `nodeName`/`compatibleNodes`/`volumeRef` | 1 |
| Objects: `Environment` same shape | 1 |
| Objects: `Volume` loses `credential_secret`, keeps `team` | 1 |
| Objects: `phase` is a Rust enum on every kind; `observedGeneration` + `conditions` everywhere | 1 |
| Objects: `SnapshotRequest` kind, `spec: {volume, message?}` only, labels, no selector | 1, 5 |
| Objects: `SnapshotRequest` finalizer `kloudlite-git.io/snapshot` | 1, 5 |
| Objects: `Volume.status.lastPush` DROPPED, nothing replaces it | 1 (schema), 5 (writer), 8 (query) |
| Objects: `OwnerBinding` keeps `observedGeneration`, gains a reconciler and `.spec.nodeName` | 1, 3 |
| Placement: unplaced watch per role, claim rules 1-3, `cloneOf` exception | 2 |
| Placement: OPTIMISTIC claim (`replace_status` + `resourceVersion`, 409 → re-read) | 2 |
| Placement: `compatibleNodes = union(existing, {me})`, never append | 2 |
| Placement: only the winner creates the OwnerBinding | 2 |
| Placement: `placement.rs` moves to `bins/agent` | 2 |
| Placement: init image pinned in the agent's env | 4 (D7), 8 (DaemonSet) |
| What triggers what (the watch table), incl. `cloneOf` and `SnapshotRequest`→`Volume` | 2, 3, 4, 5, 6 |
| Watches label-selected on `kloudlite-git.io/kind` | 4 (Pod), 6 (Deployment) |
| Reconcile flows: Workspace 1-6 | 4 |
| Reconcile flows: Environment, stop gated on a `done` child | 6 |
| Reconcile flows: Volume minus token/clone/push | 5 |
| Reconcile flows: SnapshotRequest 1-6, incl. restart→`error` and the finalizer | 5 |
| Reconcile flows: error classification, permanent vs transient | 2 (helper), 4, 5, 6 |
| Reconcile flows: OwnerBinding | 3 |
| Deletion: GC removes the Volume | 4 (ownerReference), 8 (one-call delete) |
| The API after this | 8 |
| Platform key after `Placed`, retry on list | 8 |
| RBAC, incl. `snapshotrequests` on both roles and the API losing `volumes` delete | 8 |
| Migration: two-step schema | 1 (release 1 keeps the fields), 11 (release 2 drops them) |
| Migration: ownerRefs, status backfill, SnapshotRequests from registry history | 7 |
| Migration: `ws-16980a570dd6eecd`, roll order | 10 |
| Testing: crd_yaml | 1 |
| Testing: reconcile.rs, all nine behaviours | 2, 3, 4, 5, 6, 7 |
| Testing: api_user.rs three assertions | 8 |
| Testing: ws_e2e.sh phase | 10 |

The spec's "deleting a Workspace with an in-flight push still waits" is covered twice over: the
existing `deleting_a_volume_waits_for_an_in_flight_operation`
(`bins/agent/tests/reconcile.rs:183-212`), which Task 5 must keep green, and Task 5's new
`deleting_a_working_request_waits_for_the_handle`, which covers the case the Volume's finalizer
does NOT reach — a SnapshotRequest is deliberately not the Volume's child, so before Task 1's
`SNAPSHOT_FINALIZER` a delete during `working` orphaned the send outright.

**2. Placeholder scan.** No "TBD", no "similar to Task N", no "add validation". One place names a
judgement rather than code, deliberately and bounded: Task 10 Step 2 tells the implementer to reuse
`$E2E_REPO` from the git phase already in the script — and says what to do if that phase does not
exist yet. Everything else is literal code against a file and line range the plan names.

The web half was verified against the real files rather than assumed: `placement` is already
`string | null` and read nowhere, `volume`/`live_state` likewise, `quota_gb` still resolves after
the `storage` move, and `lineage` is read in exactly one component — which Task 9 edits in the same
task that empties it.

**3. Type consistency.** `WorkspaceStorage` (not `StorageSpec`) everywhere; `status.node_name` /
`compatibleNodes` in JSON; `volume_ref: Option<String>` in status; `crd::Phase` is the phase type on
all four kinds that have one and no `phase: "…".into()` string literal survives anywhere in the
plan (checked by grep); `Volume.status` has neither `lastPush` nor `lastSnapshot`, and the one
place that answered "has this been pushed" is `api::pushed_volumes`;
`Done { phase: crd::Phase, lineage_tip }` after Task 5, used by Task 5's tests and by `volume_work`;
`SnapshotRequestSpec { volume, message }` has no `node_name` in any task — the API's
`request_snapshot`, the Environment's `await_stop_snapshot`, the migration's `backfill_history` and
every test fixture agree; `ensure_child_volume` has one signature, used by Tasks 4 and 6;
`binding::NAMESPACE_READY` is the single spelling of that condition; `crd::VOLUME_LABEL` is the
single spelling of `kloudlite-git.io/volume`, used by Tasks 1, 4, 5, 6, 7 and 8;
`controller::{Outcome, settle, replace_status}` are defined once in Task 2 and referenced by name
thereafter.

**4. Audit coverage.** Every "Must change before implementing" item from
`.superpowers/sdd/ownership-audit.md` maps to a task: (1) finalizer → Task 1 + 5; (2) the
`lastSnapshot` cross-writer → deleted outright, Tasks 1/5/8; (3) optimistic claim → Task 2;
(4) two-step schema → Tasks 1 and 11; (5) `cloneOf` watch → Task 4; (6) `snapshotrequests` RBAC and
`all_crds()` → Tasks 1 and 8; (7) verify `status.nodeName=` on real k3s → Task 1 Step 5;
(8) agent restart at `working` → Task 5. Of the "Should" items, 9-14 and 16-18 are covered above;
**15 (a max-age on the Volume and SnapshotRequest finalizers, with a `CleanupFailed` condition) is
deliberately NOT in this plan** — it is a pre-existing behaviour of `cleanup_volume`, not something
this change introduces, and folding an unbounded-wait fix into a ten-task refactor is how a
refactor stops being reviewable. It wants its own small plan; note it in the handoff.
19-21 (splitting the long reconcilers into `ensure_*` functions, PV/PVC watches, an indexed
OwnerBinding mapper) are P2 and are marked `ponytail:` where they arise.
