//! The `rustic-git.io/v1alpha1` custom resources — the reconcile substrate for workspaces and
//! environments.
//!
//! These types ARE the source of truth. `/v1` writes spec, each node's controller reconciles the
//! objects bound to it and writes status back through the `/status` subresource. Cosmos keeps only
//! cross-cluster `Region` metadata; where the two could disagree, the CRD wins, always.
//!
//! Two attributes on every kind are load-bearing and both fail SILENTLY when dropped, which is why
//! `tests/crd_yaml.rs` asserts them rather than trusting review:
//!
//! * `status = "…"` emits the `/status` subresource. Without it a status write folds into spec, and
//!   the RBAC split that stops a controller editing its own desired state becomes decorative. The
//!   split is half of that guarantee: the agent still holds `patch` on the main resources (for
//!   labels, finalizers and `VolumeSpec::restore_to`), and it is the
//!   ValidatingAdmissionPolicy in
//!   `deploy/k3s/agent-admission.yaml` that refuses it any other spec change.
//! * `selectable = "…nodeName"` emits `selectableFields`, which is what lets a controller watch
//!   only its own node's objects. Without it every node sees every object and two agents race the
//!   same subvolume. WHICH path is selectable differs by kind: placement is a fact the controllers
//!   establish, so a parent (`Workspace`, `Environment`) selects on `.status.nodeName` while a
//!   controller-written child (`Volume`) selects on `.spec.nodeName`.
//!
//! All five kinds are CLUSTER-scoped (no `namespaced` attribute): they name node-local storage, and
//! a namespace would imply a tenancy boundary the btrfs pool does not have. The pods and services
//! they produce are namespaced; the objects describing them are not.

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::{CustomResource, CustomResourceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;

pub const GROUP: &str = "rustic-git.io";
pub const VERSION: &str = "v1alpha1";
/// The controller writes status under its own manager; a server-side-apply conflict against it
/// therefore means another controller, not `/v1`.
pub const AGENT_FIELD_MANAGER: &str = "rustic-git-agent";
/// Held while a subvolume exists on a node. The object must outlive the delete request until the
/// controller has actually reclaimed the bytes — otherwise the record of what to reclaim is gone
/// before the reclaim happens.
pub const SUBVOLUME_FINALIZER: &str = "rustic-git.io/subvolume";
/// Held on a shared-volume clone workspace. A workspace that is a
/// shared-volume clone (`spec.storage.source` is `CloneOf { commit: Some(_), .. }`) checks out a
/// worktree under the SOURCE volume's `live/`, not its own — it owns no `Volume` child, so
/// nothing's ownerReference GC ever reclaims that worktree. This finalizer is what makes the
/// delete drop it. An owned-volume workspace also carries this finalizer (added uniformly to
/// avoid distinguishing the two cases before the spec's `source` is known to be gone at delete
/// time), but its cleanup is a no-op: the owned `Volume`'s own `SUBVOLUME_FINALIZER` already
/// deletes the whole voldir, worktree included.
pub const WORKTREE_FINALIZER: &str = "rustic-git.io/worktree";

/// What the operator asked for, independent of what is currently true.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum DesiredState {
    Running,
    Stopped,
}

/// Where a volume's initial content comes from. Absent means an empty subvolume.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum VolumeSource {
    /// A local snapshot of a sibling on the same pool — no registry round trip.
    ///
    /// With `commit: Some(_)`, `volume` names the SOURCE'S OWN volume (not a
    /// destination this object owns) and no child `Volume` is ever created for it: the clone is a
    /// second worktree of the same volume, checked out at `commit` — the graft point the API
    /// resolved ONCE at clone time, so the clone stays on what the caller saw rather than drifting
    /// with the source's later pushes. `None` is every clone written before shared-volume clones
    /// existed, which still copies bytes into a fresh child `Volume` via `clone_local_ids`.
    CloneOf {
        volume: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        commit: Option<String>,
    },
    /// A clone of a source whose node is DOWN: its OWN volume, seeded from a read-only copy of
    /// `snapshot` that the claiming node already holds locally (`{pool}/vol/{volume}/snap/{id}`).
    ///
    /// `CloneOf{commit: Some(_)}` cannot serve this case: it makes the clone a second worktree of
    /// the SOURCE'S volume, which is pinned to the dead node, so the peer holding the cut settles
    /// `Degraded=NodeMismatch` and the clone never starts. Here `volume` is read ONLY as the place
    /// to copy bytes from — the clone owns a fresh child `Volume` on the claiming node and takes no
    /// pin on the source's — which is why the interrupted branch of `/v1`'s clone writes this and
    /// nothing else does.
    SeededFrom { volume: String, snapshot: String },
    /// A git repository on this platform, cloned at `branch` into the fresh subvolume by the
    /// workspace pod's INIT CONTAINER, not by the agent.
    ///
    /// No credential here and none in a Secret either: the clone runs inside the workspace, over
    /// SSH, as the owner, with the platform key already mounted at `k8s::USER_KEY_PATH`. The old
    /// `credential_secret` named a Secret nobody ever wrote and the agent had no permission to
    /// read — the git-seeding path was dead code that looked wired.
    GitRepo { repo: String, branch: String },
}

/// "Put this snapshot back into the volume that is already there", as a wish rather than a verb.
///
/// The API writes it on the parent (`EnvironmentSpec::restore`); the parent's reconciler copies it
/// down to the child it owns (`VolumeSpec::restore_to`) once the services are down. It is never
/// CLEARED by a controller: a wish that is done is one whose `snapshotId` the Volume already
/// reports in `status.restoredTo`, so a second restore of the SAME snapshot is expressible — a new
/// `requestedAt` makes it a different wish.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestoreWish {
    pub snapshot_id: String,
    /// The volume the RECORD lives under, which is not always the volume being restored INTO — a
    /// restore can graft another volume's snapshot in place.
    pub volume: String,
    /// The registry owner LABEL of `volume` (a team slug for a team's environment). Absent means
    /// the destination's own owner, which is every personal restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// RFC-3339, written by the API. The only thing that distinguishes "restore this snapshot
    /// again" from "already done".
    #[serde(default)]
    pub requested_at: String,
}

#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "rustic-git.io",
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
    /// Same meaning as `WorkspaceSpec::team`; carried here because the controller materializes a
    /// volume before its workspace exists and needs the namespace for the git credential.
    #[serde(default)]
    pub team: String,
    /// Copied ONCE from the parent's `status.nodeName` when the parent's controller creates this
    /// child (`ensure_child_volume`) — the node whose `VolumeReplica` claim won (Synced decides
    /// placement now; there is no owner→node pin). A pod's affinity is derived from this and never
    /// chosen independently — two places allowed to name a node is two places that can disagree
    /// about where the data is.
    pub node_name: String,
    pub region: String,
    pub quota_gb: u64,
    /// How many nodes should hold a synced copy of this volume's snapshots — the replacement
    /// for "one node has the only bytes". Defaulted so every `Volume` written
    /// before this field existed keeps parsing; the reconciler that creates `VolumeReplica`
    /// children treats a missing field the same as an explicit 2.
    #[serde(default = "default_replicas")]
    pub replicas: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<VolumeSource>,
    /// Written by the PARENT's reconciler, never by a user: restoring in place under a running
    /// service is how a database ends up with a half-old disk, so the parent scales down first and
    /// only then asks for this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_to: Option<RestoreWish>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VolumeStatus {
    pub phase: Phase,
    /// The snapshot id last materialized INTO `live`. `spec.restoreTo.snapshotId` == this is the
    /// whole "already done" test, on both sides: the Volume does not restore again and the parent
    /// scales its services back up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restored_to: Option<String>,
    /// The `requestedAt` of the wish that put `restoredTo` there. Both halves, or restoring the
    /// SAME snapshot a second time is a silent no-op — which is exactly what someone does after
    /// undoing a restore by hand, or after a bad afternoon of changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_requested_at: Option<String>,
    /// Stamped from `metadata.generation` so a reconcile can tell "already done" from "not yet
    /// seen" — the difference between an idle requeue and a duplicated btrfs send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default)]
    pub subvolume_present: bool,
    // No `lastSnapshot` and no `lastPush`: "the newest snapshot of this volume" is a query over
    // `Snapshot` CRs by the `rustic-git.io/volume` label. A second controller writing this
    // status object would prune the first one's fields — `patch_status` applies FORCED under one
    // `AGENT_FIELD_MANAGER`, and server-side apply removes fields a manager previously owned and no
    // longer sets, so the Volume reconciler's very next pass would delete whatever the snapshot
    // reconciler had just written.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

/// A region a workspace may run in — cluster-scoped, like every other kind here.
///
/// Cross-cluster metadata by nature, and it used to live in Cosmos for exactly that reason. It does
/// not need to: a region is registered by an admin, read on every create, and changed almost never,
/// so the cheapest correct home is the API server this tier already talks to. `spec.status` is
/// DESIRED state (`active`/`inactive`) — re-registering is the only way to retire one, and a
/// retired region stops being offered while its existing workspaces keep running.
#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "rustic-git.io",
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

/// One immutable cut — a snapshot or a sync point: a btrfs RO subvolume, recorded as a CR before the snapshot is cut so a
/// retry finds the object and continues rather than orphaning a subvolume.
///
/// Never patched once `status.phase == Ready` — a `Snapshot` is a fact about the past, and the
/// only two things that ever remove one are an explicit delete and GC, same discipline as a
/// registry blob. Replaces the older `SnapshotRequest` push-as-annotation kind, which the cutover
/// task has since deleted — this is the only push record left.
#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "rustic-git.io",
    version = "v1alpha1",
    kind = "Snapshot",
    plural = "snapshots",
    shortname = "snp",
    status = "SnapshotStatus",
    selectable = ".spec.volume",
    printcolumn = r#"{"name":"Volume","type":"string","jsonPath":".spec.volume"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#,
    derive = "PartialEq"
)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotSpec {
    pub volume: String,
    pub owner: String,
    /// The Workspace/Environment id whose worktree this cut is taken FROM — a volume can have
    /// more than one worktree (a workspace plus a clone still attached, say), so `spec.volume`
    /// alone does not say which one to snapshot; the creator (Task 6's `/push`) names it. The
    /// snapshot reconciler only acts when THIS field's worktree is the one running on its node.
    /// Required, no default: `Snapshot` is a brand-new, flag-gated kind — there are no stored
    /// objects predating this field, so the usual back-compat exemption does not apply here.
    pub worktree: String,
    /// The parent cut's name, or empty for a root. Order comes ONLY from this chain — nothing
    /// reads creation timestamps to reconstruct history.
    #[serde(default)]
    pub parent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// A sync point, not a push: cut by the agent's sync beat (or a stop) from a live worktree so a
    /// replica holds its latest state. Never a `parent` of anything, never a worktree's `head`, and
    /// retained ONE per worktree — see `snapshot::retain`. `push` never sets this, which is the
    /// whole distinction: `!transient` IS a snapshot (`Snapshot::is_snapshot`), and it is the only
    /// one — an older build wrote a second flag alongside it, which serde ignores on the objects
    /// stored while it existed.
    #[serde(default)]
    pub transient: bool,
    /// Absent only on a snapshot cut before 2026-09-03; every reader falls back for `None`.
    ///
    /// Schema is hand-written as free-form JSON (`preserve_unknown_state`), not `SnapshotState`'s
    /// own derived schema: kube-core's CRD generation flattens an internally-tagged enum's `oneOf`
    /// branches into one object and panics when a shared property (`kind`) carries a different
    /// `const` per branch — which is the entire point of a tag. A hand-written *discriminated*
    /// schema would drift from the type the moment a variant changes, so this stays a plain
    /// `x-kubernetes-preserve-unknown-fields` object instead of trying to describe the union.
    /// `serde`'s view of the Rust type is untouched, so round-tripping is exact; only the
    /// *published* OpenAPI schema for this field is permissive. Because the schema can't validate
    /// it, a value that doesn't parse as `SnapshotState` must not fail the whole `Snapshot` read —
    /// `lenient_state` drops it to `None` (with a `tracing::warn!`) rather than taking the agent's
    /// list/watch down with it.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "lenient_state")]
    #[schemars(schema_with = "preserve_unknown_state")]
    pub state: Option<SnapshotState>,
}

fn preserve_unknown_state(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    serde_json::from_value(serde_json::json!({
        "type": "object",
        "x-kubernetes-preserve-unknown-fields": true
    }))
    .expect("static schema literal")
}

/// `state` is unvalidatable by the published schema (see `preserve_unknown_state`), so a value
/// that doesn't parse as `SnapshotState` — hand-edited, or written by some future variant this
/// build doesn't know — must not fail the whole `Snapshot` read. Every reader already treats
/// `None` as "no frozen state, fall back to defaults", which is exactly right for "couldn't read
/// it" too. The trap this sets for tests: a PARTIAL value (`resources: {}` with `PodResources`'
/// required fields missing) also becomes `None`, silently — a fixture that meant "a workspace
/// state" reads as "no state", and a test asserting on the kind passes or fails for the wrong
/// reason. Build fixtures with every field.
fn lenient_state<'de, D>(deserializer: D) -> Result<Option<SnapshotState>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(raw.and_then(|v| match serde_json::from_value(v.clone()) {
        Ok(state) => Some(state),
        Err(e) => {
            tracing::warn!(error = %e, value = %v, "snapshot state did not parse; treating as absent");
            None
        }
    }))
}

/// The legacy-Volume quota fallback (`FALLBACK_QUOTA_GB` in `api.rs`) — was already `20`, named
/// here so `SnapshotState::of_workspace` and `api.rs` share the one number.
pub const DEFAULT_WS_QUOTA_GB: u64 = 20;
/// `default_env_quota()`'s value in `api.rs` — both the `NewEnvironment.quota_gb` request-body
/// default (an environment created without one gets this) and `SnapshotState::of_environment`'s
/// fallback for a legacy `spec.storage`-less object; was already `20`, named here to share it.
pub const DEFAULT_ENV_QUOTA_GB: u64 = 20;

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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotStatus {
    /// `Working` until the btrfs subvolume is actually cut; `Ready` is the point past which the
    /// object is immutable.
    pub phase: Phase,
    /// When `phase` became `Ready`, RFC3339 — `lastTransitionTime` on a condition would do, but a
    /// `Snapshot` has no conditions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<String>,
}

/// One node's copy of a volume's snapshots — the per-node replica state kept in place of "the object store has the only bytes".
///
/// Written only by `spec.node`'s own controller, with two guarded exceptions: deleting a dead
/// node's replica row and clearing a dead node's claims, both gated on that node being NotReady
/// for longer than `WS_NODE_DEAD_SECS`.
#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "rustic-git.io",
    version = "v1alpha1",
    kind = "VolumeReplica",
    plural = "volumereplicas",
    shortname = "vr",
    status = "VolumeReplicaStatus",
    selectable = ".spec.node",
    selectable = ".status.phase",
    // Scopes only `replicated_condition`'s per-reconcile replica list (`controller/stop.rs`) —
    // `pull_volume` filters Snapshots by `spec.volume`, a different kind, not this one. Apply
    // `deploy/k3s/crds.yaml` BEFORE rolling an agent that uses it: an unsupported field selector
    // is a 400, and every stopped parent's `Replicated` recompute then errors — not replication.
    selectable = ".spec.volume",
    printcolumn = r#"{"name":"Volume","type":"string","jsonPath":".spec.volume"}"#,
    printcolumn = r#"{"name":"Node","type":"string","jsonPath":".spec.node"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#,
    derive = "PartialEq"
)]
#[serde(rename_all = "camelCase")]
pub struct VolumeReplicaSpec {
    pub volume: String,
    pub node: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VolumeReplicaStatus {
    /// "Synced" | "Syncing" — a plain `String`, not `Phase`: this is a `selectableField` and the
    /// API server only accepts a string type there, never an enum's underlying representation.
    pub phase: String,
    /// Branch name to snapshot id, this node's own view — what a reader checks before trusting a
    /// `head` claim against this replica.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub branches: BTreeMap<String, String>,
}

/// The message an older build stamped on a migration baseline, back when a baseline was written as
/// an ordinary record. Matched by shape rather than migrated: the records are already on the
/// cluster, and a migration job to rewrite them is more machinery than one predicate.
const LEGACY_BASELINE_MESSAGE: &str = "migration baseline";

impl Snapshot {
    /// A push, as opposed to a sync point — the one distinction there is. Everything that keeps a
    /// record (retention, `cleanup_parent`, the volume listing, `delete_snapshot`) asks this.
    ///
    /// A MIGRATION BASELINE is not a push either, whoever wrote it: nobody asked for it, it exists
    /// only to seed replication of a pre-model volume, and treating one as a snapshot would keep
    /// its Volume alive forever after the workspace was deleted. Baselines are written as sync
    /// points now (`migrate_and_seed_baseline`); the shape match is for the ones already stored.
    pub fn is_snapshot(&self) -> bool {
        !self.spec.transient && !self.is_legacy_baseline()
    }

    // ponytail: a baseline is recognised by SHAPE — a root record of the volume's own worktree
    // carrying exactly that message — because the records are already stored and a rewrite job is
    // more machinery than a predicate. Ceiling: a person's very first push, on the volume's own
    // worktree, whose message is exactly "migration baseline", reads as one and would be deleted
    // with its parent. Upgrade path: a `baseline: true` field on new records, and this shape match
    // kept only for the pre-field ones.
    fn is_legacy_baseline(&self) -> bool {
        self.spec.parent.is_empty()
            && self.spec.worktree == self.spec.volume
            && self.spec.message.as_deref() == Some(LEGACY_BASELINE_MESSAGE)
    }
}

/// `{volume}-{8 hex}` — CR-first naming: minted before the btrfs snapshot is cut, so a retried
/// create finds this same object rather than a new one. Random, not sequential, because ORDER
/// comes only from `SnapshotSpec::parent`, never from the name.
pub fn snapshot_name(volume: &str) -> String {
    format!("{volume}-{}", short_hex())
}

/// `{volume}.{node}` — deterministic so two callers naming the same volume/node pair always agree
/// on the one `VolumeReplica` object, rather than racing to create duplicates.
pub fn replica_name(volume: &str, node: &str) -> String {
    format!("{volume}.{node}")
}

/// Four random bytes as 8 lowercase hex characters — the same `rand`-backed shape `api::rid` uses
/// for every other object id in this crate, kept local because a `Snapshot` name is not prefixed.
pub fn short_hex() -> String {
    use rand::RngCore;
    let mut b = [0u8; 4];
    rand::thread_rng().fill_bytes(&mut b);
    rustic_git_core::hex(&b)
}

/// Requests and limits for a workspace pod, as plain strings in Kubernetes quantity form.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PodResources {
    pub cpu_request: String,
    pub cpu_limit: String,
    pub memory_request: String,
    pub memory_limit: String,
}

impl Default for PodResources {
    /// The "M" session slot from the capacity model: guarantee 4 GB / 2 vCPU, limit 8 GB / 4 vCPU.
    ///
    /// The REQUEST is the load-bearing half. It is what the scheduler packs against, so it — not
    /// the limit — decides how many sessions a node holds and therefore what a session costs. The
    /// previous 512Mi/250m request was a small floor "with room to burst", which let a 128 GB node
    /// accept roughly 235 sessions against the model's ~30, and made the model's "guaranteed CPU is
    /// NOT oversubscribed on session nodes" false: 235 × 2 vCPU of promised capacity on 64 vCPU.
    ///
    /// The arithmetic these numbers have to satisfy, on a 32-OCPU / 128 GB session node at the
    /// model's 94% usable-memory headroom: 120 GB ÷ 4 GB = 30 sessions, needing 30 × 2 = 60 vCPU of
    /// 64. Memory-bound, CPU fits, guarantee honoured.
    fn default() -> Self {
        Self {
            cpu_request: "2".into(),
            cpu_limit: "4".into(),
            memory_request: "4Gi".into(),
            memory_limit: "8Gi".into(),
        }
    }
}

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
    /// The owning node is dead and the pin has been cleared: no node may write this subvolume
    /// until one takes it (`resolve_volume`'s takeover arm). Distinct from `Error` so an
    /// operator can tell "waiting for a Synced survivor" from "something is broken".
    Unavailable,
    /// Historical: a pre-cutover `SnapshotRequest` whose record was in the registry, never
    /// re-run past this. The kind is gone; the variant stays for CRs written before the cutover.
    Done,
    Error,
}

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
            Phase::Unavailable => "unavailable",
            Phase::Done => "done",
            Phase::Error => "error",
        }
    }
}

#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "rustic-git.io",
    version = "v1alpha1",
    kind = "Workspace",
    plural = "workspaces",
    shortname = "ws",
    status = "WorkspaceStatus",
    // Placement is a FACT the controllers establish, so it lives in status — and a status path is
    // a legal selectable field (only metadata is forbidden, and arrays are not allowed). An empty
    // value is what the unplaced watch selects on.
    selectable = ".status.nodeName",
    // `parents_of_volume` asks "what is running on this volume" on every snapshot and volume
    // delete; without this it was two full-cluster lists per question. An unset value indexes as
    // the empty string, which is what makes "not placed yet" its own queryable set.
    selectable = ".status.volumeRef",
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
    /// Optional so an object created before this field existed still PARSES, rather than the
    /// controller 422ing every Workspace it tries to write. A missing one is a permanent
    /// `NoStorage` failure on the reconcile — nothing can build a disk without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<WorkspaceStorage>,
    pub desired_state: DesiredState,
    #[serde(default)]
    pub resources: PodResources,
    /// The package list, written by the API. Lives on `spec`, not a file in the workspace's own
    /// subvolume: one object, one list — a clone copies it for free along with the rest of spec,
    /// and a restore (which grafts onto a past snapshot of the volume) never touches it, because
    /// spec is not part of what a restore replaces.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<String>,
    /// The environment whose services this workspace resolves by bare name, or `None`.
    ///
    /// One, not a list: bare-name resolution has to be unambiguous, and two attached environments
    /// both exposing `db` would let search-domain order silently pick the winner.
    ///
    /// Written only by `/v1` — the agent's admission policy forbids it writing spec, and a stale
    /// id here is not an error: the reconciler treats a missing or wrong-region environment as
    /// unattached rather than leaving a grant behind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached_environment: Option<String>,
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
    /// DEAD as of the 2026-09-03 stop/decommission design: placement reads the replica rows'
    /// `branches` now, so "who held this once" is never consulted. Kept as a tolerated field (not
    /// declared in the schema) so a stored object written before the cutover still parses;
    /// nothing writes it and nothing reads it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(skip)]
    pub compatible_nodes: Vec<String>,
    /// The child `Volume`, reported rather than wished for: the reconciler creates it and then
    /// says so here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    /// The package profile actually converged, reported rather than wished for — `spec.packages`
    /// carries the list the reconciler last saw; this is what building it produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packages: Option<PackagesStatus>,
    /// The pod's SSH public host key, reported by the node once sshd's key exists. The CLI pins
    /// it in `known_hosts`, so an absent one means "no session yet", never "trust on first use".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_host_key: Option<String>,
    /// The snapshot id this worktree is checked out on right now. Written ONLY by the node actually
    /// running the pod — no other node can observe it, and a stale value here is exactly what
    /// "the pod moved and hasn't reconciled yet" looks like, never a fact anyone else may act on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
}

/// What the reconciler last saw and built from `spec.packages`: `observed` and `observed_hash`
/// are the LIST as of the last pass (the hash is the idempotency key — a rebuild is skipped when
/// it still matches), while `profile` is the Nix store path the profile on disk actually
/// resolved to.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PackagesStatus {
    /// The platform's base set the profile was built with, on top of `observed`. Reported so a
    /// page can show what every workspace gets without asking the node which env it runs with.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub base: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nixpkgs: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatus {
    pub name: String,
    pub ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "rustic-git.io",
    version = "v1alpha1",
    kind = "Environment",
    plural = "environments",
    shortname = "env",
    status = "EnvironmentStatus",
    // Placement is a FACT the controllers establish, so it lives in status — and a status path is
    // a legal selectable field (only metadata is forbidden, and arrays are not allowed). An empty
    // value is what the unplaced watch selects on.
    selectable = ".status.nodeName",
    // `parents_of_volume` asks "what is running on this volume" on every snapshot and volume
    // delete; without this it was two full-cluster lists per question. An unset value indexes as
    // the empty string, which is what makes "not placed yet" its own queryable set.
    selectable = ".status.volumeRef",
    printcolumn = r#"{"name":"Owner","type":"string","jsonPath":".spec.owner"}"#,
    printcolumn = r#"{"name":"Node","type":"string","jsonPath":".status.nodeName"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#,
    derive = "PartialEq"
)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentSpec {
    /// A team, usually — environments are team-owned, workspaces are user-owned.
    pub owner: String,
    pub name: String,
    pub region: String,
    /// Reused verbatim from the domain model: the same `Service`/`Mount` the `/v1` API has always
    /// taken, so a mount is still validated by `model::validate_mount` before it becomes a volume.
    pub services: Vec<crate::model::Service>,
    /// Optional in release 1, same reason as `WorkspaceSpec::storage`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<WorkspaceStorage>,
    pub desired_state: DesiredState,
    /// The user's wish to put a past snapshot back into THIS environment's own disk, rather than
    /// into a new one. Additive and never cleared by a controller — see `RestoreWish`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<RestoreWish>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentStatus {
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// Where this object runs NOW. Empty means unplaced, which is exactly what the placement
    /// watch's `status.nodeName=` field selector matches.
    #[serde(default)]
    pub node_name: String,
    /// DEAD as of the 2026-09-03 stop/decommission design: placement reads the replica rows'
    /// `branches` now, so "who held this once" is never consulted. Kept as a tolerated field (not
    /// declared in the schema) so a stored object written before the cutover still parses;
    /// nothing writes it and nothing reads it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(skip)]
    pub compatible_nodes: Vec<String>,
    /// The child `Volume`, reported rather than wished for: the reconciler creates it and then
    /// says so here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub service_status: Vec<ServiceStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    /// Same meaning and same one-writer rule as `WorkspaceStatus::head` — the snapshot id this
    /// environment's worktree is checked out on, written only by the node running it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// The `spec.restore` wish this environment has already applied, recorded as the same PAIR the
    /// `Volume` records (`restoredTo` + `restoreRequestedAt`) and for the same reason: restoring
    /// the same snapshot twice is a legitimate ask, so the id alone cannot tell a fresh wish from
    /// a granted one.
    ///
    /// It exists because a granted wish stays in the spec forever — a controller does not edit the
    /// user's desired state. Without a record of having applied it, `restore_gate` re-derives
    /// `head` from the wish on EVERY pass, which silently undoes every push: the snapshot
    /// reconciler advances `head`, the next reconcile stamps it back to the restore point, and the
    /// environment's history can never move past the snapshot it was last restored to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restored_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_requested_at: Option<String>,
}

/// Which owner has namespaces reconciled, per region. One object per `{region, owner}`, and every
/// node reconciles every binding — the home is a region-shared NFS directory, so a binding is not
/// node-scoped (it once pinned an owner to a node; that pin is gone).
#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "rustic-git.io",
    version = "v1alpha1",
    kind = "OwnerBinding",
    plural = "ownerbindings",
    shortname = "ob",
    status = "OwnerBindingStatus",
    printcolumn = r#"{"name":"Owner","type":"string","jsonPath":".spec.owner"}"#,
    printcolumn = r#"{"name":"Region","type":"string","jsonPath":".spec.region"}"#,
    derive = "PartialEq"
)]
#[serde(rename_all = "camelCase")]
pub struct OwnerBindingSpec {
    pub owner: String,
    pub region: String,
}

/// Two: one active copy plus one standby, the smallest number that survives a single node loss.
pub const DEFAULT_REPLICAS: u32 = 2;
fn default_replicas() -> u32 {
    DEFAULT_REPLICAS
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OwnerBindingStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

/// The label a `Snapshot` carries so `/v1/volumes/{id}/history` is one indexed list
/// call rather than a scan. Same rule as every other label here: a VIEW of `spec.volume`, never
/// authorization.
pub const VOLUME_LABEL: &str = "rustic-git.io/volume";

/// Set on a `Node` by an operator (`kubectl label node <n> rustic-git.io/decommission=true`) to
/// retire it. A LABEL and not an annotation because it is a selector-worthy fact about the node,
/// and because removing it is the documented abort. Only the exact value `"true"` counts: a
/// half-typed label must never drain a node.
pub const DECOMMISSION_LABEL: &str = "rustic-git.io/decommission";

/// Labels every `Snapshot`/`VolumeReplica` create site stamps: `spec.volume`/`spec.owner` restated
/// as labels so a watch or a list (the e2e's `-l rustic-git.io/volume=...`, `/v1`'s own reads) can
/// select on them — a label cannot be queried out of an arbitrary spec field. A VIEW, same rule as
/// every other label in this file: `spec` stays the truth, this is never read for authorization.
pub fn commit_labels(owner: &str, volume: &str) -> std::collections::BTreeMap<String, String> {
    std::collections::BTreeMap::from([
        ("rustic-git.io/owner".to_string(), owner.to_string()),
        (VOLUME_LABEL.to_string(), volume.to_string()),
    ])
}

/// Names the Environment a `stop-{env}` request belongs to, so the environments controller can
/// watch only those instead of every push in the cluster. Also a view: the ownerReference is the
/// link the mapper reads, and this label exists only because a watch cannot select on one.
pub const STOP_LABEL: &str = "rustic-git.io/stop-of";

/// Has the Volume already granted this exact wish?
///
/// Both halves of the pair, and the ONE place that decides it — the Volume's own guard and its
/// parent's gate must never disagree about whether a restore is finished, or one scales services
/// back up while the other still means to swap the disk under them.
pub fn wish_granted(wish: &RestoreWish, restored_to: Option<&str>, restored_at: Option<&str>) -> bool {
    restored_to == Some(wish.snapshot_id.as_str()) && restored_at == Some(wish.requested_at.as_str())
}

/// The RFC-1123 object name for an owner's node binding: `{region}-{owner}` plus a hash tail
/// over the PAIR. Region ids and handles both allow `-`, so the bare join was ambiguous —
/// `centralindia-x` + `att` and `centralindia` + `x-att` — and the tail is what tells them apart.
pub fn binding_name(region: &str, owner: &str) -> String {
    let (region, owner) = (region.to_lowercase(), owner.to_lowercase());
    dns_label(&format!("{region}-{owner}-{}", pair_tail(&region, &owner)))
}

/// Twelve hex characters of sha256 over `"{a}/{b}"`. `/` is the separator because no handle,
/// team slug or region id can contain it, which is what makes the pre-image — and so the tail —
/// distinct for distinct pairs.
fn pair_tail(a: &str, b: &str) -> String {
    hex_prefix(&format!("{a}/{b}"), 6)
}

fn hex_prefix(raw: &str, bytes: usize) -> String {
    use sha2::Digest;
    hex::encode(&sha2::Sha256::digest(raw.as_bytes())[..bytes])
}

/// The namespace ALL of an owner's workspace pods live in — one per user, not one per workspace.
///
/// Shared on purpose: it keeps the object count proportional to users rather than to workspaces,
/// and it gives a per-user `ResourceQuota` somewhere to live, which is the unit a limit is
/// naturally expressed in ("this user gets N CPUs across everything they run").
///
/// Two consequences follow and are handled where they arise, not here: the namespace must carry NO
/// `ownerReference` (deleting one workspace would otherwise garbage-collect the namespace and every
/// sibling in it), and an attachment must select the individual workspace's POD rather than the
/// whole namespace.
///
/// Personal is `ws-{owner}`; a team pair is `wt-{owner}-{tail}`, the tail hashed over
/// `(team, owner)`. Not `ws-{team}-{owner}`: handles and team slugs both allow `-`, so team
/// `acme` with owner `bob` and the personal namespace of handle `acme-bob` were ONE namespace, and the
/// fixed-name `user-key` Secret in it — the owner's private git key — was shared between two
/// people. A distinct prefix keeps team namespaces out of the personal keyspace entirely, and the
/// tail keeps two pairs apart without a separator a handle could forge. The longest case is
/// `wt-` + 39 + `-` + 12 = 55 characters, so a team name never reaches `dns_label`'s truncation.
pub fn ws_namespace(owner: &str, team: &str) -> String {
    let owner = owner.to_lowercase();
    if team.is_empty() || team.eq_ignore_ascii_case(&owner) {
        return dns_label(&format!("ws-{owner}"));
    }
    dns_label(&format!("wt-{owner}-{}", pair_tail(&team.to_lowercase(), &owner)))
}

/// A namespace name is an RFC 1123 label: 63 characters at most. Two 39-character handles and
/// the prefix can reach 82, so a long pair is cut and given a hash tail — the tail is what keeps
/// two pairs that share a prefix apart. Deterministic, so the controller and the API agree.
fn dns_label(raw: &str) -> String {
    if raw.len() <= 63 {
        return raw.to_string();
    }
    let tail = hex_prefix(raw, 4);
    let head = raw[..63 - tail.len() - 1].trim_end_matches('-');
    format!("{head}-{tail}")
}

/// The namespace an environment's deployments and services live in. One namespace per environment
/// is what makes a default-deny NetworkPolicy the isolation boundary.
///
/// Idempotent, because environment ids are already minted as `env-{hex}` (`api::rid("env")`) and
/// prefixing unconditionally produced `env-env-{hex}` — valid, and wrong every time anyone read it.
/// Written this way rather than by dropping the prefix so an id whose shape changes still lands in
/// a namespace that says what it is.
pub fn env_namespace(id: &str) -> String {
    let id = id.to_lowercase();
    format!("env-{}", id.strip_prefix("env-").unwrap_or(&id))
}

/// Every CRD this repo owns, for YAML generation and for a startup precondition check.
pub fn all_crds() -> Vec<CustomResourceDefinition> {
    vec![
        Volume::crd(),
        Workspace::crd(),
        Environment::crd(),
        OwnerBinding::crd(),
        Snapshot::crd(),
        VolumeReplica::crd(),
        Region::crd(),
    ]
}

/// Condition type set once `status.packages` reflects a successful build of `spec.packages` —
/// named here, not in the agent, because it describes a status field this file owns rather than
/// a controller-local fact like `NAMESPACE_READY`.
pub const PACKAGES_READY: &str = "PackagesReady";

/// Condition type carrying the environment a workspace is attached to, in its MESSAGE (the bare
/// id). Named here rather than in the agent because `/v1` reads it back too: it is the only record
/// of which environment's namespace holds a workspace's ingress half once `spec` has been cleared.
pub const ATTACHED: &str = "Attached";

/// The environment whose namespace holds this workspace's ingress half: what the spec asks for, or
/// — once a detach has already cleared it — what the last converged pass recorded. Both, because a
/// detach on a STOPPED workspace never reaches a reconcile, so `/v1` is the only thing left that
/// can collect the grant.
pub fn attached_environment(w: &Workspace) -> Option<String> {
    if let Some(env) = w.spec.attached_environment.clone() {
        return Some(env);
    }
    w.status
        .as_ref()?
        .conditions
        .iter()
        .find(|c| c.type_ == ATTACHED && c.status == "True")
        .map(|c| c.message.clone())
        .filter(|m| !m.is_empty())
}

/// A standard condition with `observedGeneration` stamped.
///
/// `meta/v1.Condition` rather than a bespoke struct, because it is the shape
/// `kubectl wait --for=condition=Ready` already reads.
pub fn condition(kind: &str, status: bool, reason: &str, message: &str, generation: i64) -> Condition {
    condition_since(None, kind, status, reason, message, generation)
}

/// The same, keeping `prev`'s `lastTransitionTime` when nothing actually transitioned. The field
/// means "since when has it been in THIS state" — restamping it on every identical write turns
/// "failing for an hour" into "failing since a moment ago", which is exactly the signal a backoff
/// reads.
pub fn condition_since(
    prev: Option<&Condition>,
    kind: &str,
    status: bool,
    reason: &str,
    message: &str,
    generation: i64,
) -> Condition {
    let mut c = condition_now(kind, status, reason, message, generation);
    if let Some(p) = prev {
        if p.status == c.status && p.reason == c.reason {
            c.last_transition_time = p.last_transition_time.clone();
        }
    }
    c
}

fn condition_now(kind: &str, status: bool, reason: &str, message: &str, generation: i64) -> Condition {
    Condition {
        type_: kind.to_string(),
        status: if status { "True" } else { "False" }.to_string(),
        reason: reason.to_string(),
        message: message.to_string(),
        observed_generation: Some(generation),
        // The API server rejects a condition with no transition time, and a reconcile has no
        // better clock than now. `jiff`, not chrono: k8s-openapi 0.28 wraps `jiff::Timestamp`
        // here, so this is the one place in the crate that does not use the workspace's chrono.
        last_transition_time: k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
            k8s_openapi::jiff::Timestamp::now(),
        ),
    }
}

/// The btrfs generation the sync beat replicated, stamped by the owner AFTER it cuts (taking a
/// read-only snapshot bumps the source subvolume's own generation, so the pre-cut value leaves
/// every idle worktree permanently "due"). It lives here, beside `Snapshot`, because both the
/// agent that writes it and `/v1` — which must order the same transients to pick a clone's parent
/// — read it, and two copies of the ordering key is how two tiers disagree about which cut is
/// newest.
pub const SYNCED_GENERATION: &str = "rustic-git.io/synced-generation";

/// Public so the replica writer can apply the SAME key to the subset it actually holds on disk.
pub fn transient_generation_of(s: &Snapshot) -> u64 {
    use kube::ResourceExt;
    s.annotations().get(SYNCED_GENERATION).and_then(|g| g.parse::<u64>().ok()).unwrap_or(0)
}

/// The newest Ready transient of `worktree` among `snaps` — ordered by `SYNCED_GENERATION`, never
/// by creation time, because the annotation is the btrfs generation actually replicated and it is
/// the one ordering that survives clock skew between the owner that cut it and the node that
/// pulled it. A stop or clone cut carries no annotation until the owner stamps it post-cut and so
/// reads as 0: it loses to any annotated one and still beats nothing. Ties break by NAME so two
/// nodes computing this independently never disagree.
pub fn newest_transient_of(snaps: &[Snapshot], worktree: &str) -> Option<String> {
    use kube::ResourceExt;
    snaps
        .iter()
        .filter(|s| {
            s.spec.transient && s.spec.worktree == worktree && s.status.as_ref().is_some_and(|st| st.phase == Phase::Ready)
        })
        .map(|s| (transient_generation_of(s), s.name_any()))
        .max()
        .map(|(_, name)| name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `hex` is already a workspace dependency; a hand-rolled `format!("{b:02x}")` fold is the same
    /// bytes with more places to get it wrong. The tail must not move — it is in stored object names.
    #[test]
    fn the_namespace_tail_is_unchanged_by_the_hex_swap() {
        assert_eq!(ws_namespace("bob", "acme"), "wt-bob-2e737765961a");
    }

    /// The backoff on a repeatedly failing build reads `lastTransitionTime` to know how long it
    /// has been failing — so an identical condition written again must keep the earlier stamp,
    /// and a changed reason must not.
    #[test]
    fn a_repeated_condition_keeps_the_time_it_first_transitioned() {
        let mut first = condition("PackagesReady", false, "BuildFailed", "boom", 1);
        first.last_transition_time = k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
            k8s_openapi::jiff::Timestamp::UNIX_EPOCH,
        );
        let again = condition_since(Some(&first), "PackagesReady", false, "BuildFailed", "boom again", 2);
        assert_eq!(again.last_transition_time, first.last_transition_time);
        assert_eq!(again.message, "boom again");
        let changed = condition_since(Some(&first), "PackagesReady", true, "Built", "ok", 2);
        assert_ne!(changed.last_transition_time, first.last_transition_time);
    }

    #[test]
    fn workspace_status_carries_packages_and_omits_it_when_unset() {
        let st = WorkspaceStatus::default();
        assert!(!serde_json::to_string(&st).unwrap().contains("packages"));
        let st = WorkspaceStatus {
            packages: Some(PackagesStatus {
                base: vec![],
                observed: vec!["go".into()],
                observed_hash: Some("sha256:x".into()),
                profile: None,
                nixpkgs: None,
            }),
            ..Default::default()
        };
        let v = serde_json::to_value(&st).unwrap();
        assert_eq!(v["packages"]["observed"][0], "go");
        assert_eq!(v["packages"]["observedHash"], "sha256:x");
    }

    #[test]
    fn workspace_spec_carries_packages_and_omits_it_when_empty() {
        let mut spec = WorkspaceSpec {
            owner: "o".into(),
            team: String::new(),
            name: "n".into(),
            region: "r".into(),
            image: "i".into(),
            storage: None,
            desired_state: DesiredState::Running,
            resources: PodResources::default(),
            packages: vec![],
            attached_environment: None,
        };
        assert!(!serde_json::to_string(&spec).unwrap().contains("packages"));
        spec.packages = vec!["go".into(), "jq".into()];
        let v = serde_json::to_value(&spec).unwrap();
        assert_eq!(v["packages"][0], "go");
        let back: WorkspaceSpec = serde_json::from_value(v).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn a_volume_without_replicas_reads_the_default_of_two() {
        let v: VolumeSpec = serde_json::from_value(serde_json::json!({
            "owner": "alice", "nodeName": "n", "region": "r1", "quotaGb": 2
        }))
        .unwrap();
        assert_eq!(v.replicas, 2);
    }

    #[test]
    fn snapshot_name_is_volume_dash_eight_hex_and_varies_per_call() {
        let a = snapshot_name("myvol");
        let b = snapshot_name("myvol");
        assert!(a.starts_with("myvol-"), "{a}");
        let hex = a.strip_prefix("myvol-").unwrap();
        assert_eq!(hex.len(), 8);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two calls must not collide");
    }

    /// The one distinction there is: a push is a snapshot, a sync point is not, and a MIGRATION
    /// BASELINE is not — including the ones an older build wrote as ordinary records, which are
    /// already on the cluster and would otherwise keep their Volume alive forever.
    #[test]
    fn a_push_is_a_snapshot_but_a_sync_point_or_a_baseline_is_not() {
        let snap = |spec: serde_json::Value| -> Snapshot {
            serde_json::from_value(serde_json::json!({
                "apiVersion": "rustic-git.io/v1alpha1", "kind": "Snapshot",
                "metadata": {"name": "v-aaaaaaaa"}, "spec": spec,
            }))
            .unwrap()
        };
        let base = serde_json::json!({"volume": "v", "owner": "o", "worktree": "v", "parent": ""});

        assert!(snap(base.clone()).is_snapshot(), "a push");
        let mut with_msg = base.clone();
        with_msg["message"] = serde_json::json!("wip");
        assert!(snap(with_msg).is_snapshot(), "a push with a message is still a push");

        let mut transient = base.clone();
        transient["transient"] = serde_json::json!(true);
        assert!(!snap(transient).is_snapshot(), "a sync point");

        let mut legacy = base.clone();
        legacy["message"] = serde_json::json!("migration baseline");
        assert!(!snap(legacy).is_snapshot(), "a baseline an older build wrote as an ordinary record");

        // Shape, not text alone: a push that happens to carry that message but sits on a parent is
        // somebody's snapshot, and a baseline is always a root.
        let mut lookalike = base;
        lookalike["message"] = serde_json::json!("migration baseline");
        lookalike["parent"] = serde_json::json!("v-bbbbbbbb");
        assert!(snap(lookalike).is_snapshot(), "a rooted record is never a baseline");
    }

    #[test]
    fn replica_name_is_deterministic_per_volume_and_node() {
        assert_eq!(replica_name("myvol", "node-a"), "myvol.node-a");
        assert_eq!(replica_name("myvol", "node-a"), replica_name("myvol", "node-a"));
    }

    #[test]
    fn snapshot_spec_round_trips_with_empty_parent_and_no_message() {
        let spec = SnapshotSpec {
            volume: "v".into(), owner: "alice".into(), worktree: "ws-1".into(), parent: String::new(), message: None, transient: false, state: None,
        };
        let v = serde_json::to_value(&spec).unwrap();
        assert!(!v.as_object().unwrap().contains_key("message"));
        let back: SnapshotSpec = serde_json::from_value(v).unwrap();
        assert_eq!(back, spec);
    }

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
            "volume": "v", "owner": "o", "worktree": "v", "parent": "", "transient": false
        }))
        .unwrap();
        assert!(s.state.is_none());
        // and a None state is not written at all
        assert!(serde_json::to_value(&s).unwrap().get("state").is_none());
    }

    #[test]
    fn a_malformed_snapshot_state_is_dropped_not_a_deserialize_error() {
        let s: SnapshotSpec = serde_json::from_value(serde_json::json!({
            "volume": "v", "owner": "o", "worktree": "v", "parent": "",
            "transient": false, "state": {"kind": "bogus"}
        }))
        .unwrap();
        assert!(s.state.is_none());
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

    #[test]
    fn snapshot_status_round_trips_and_omits_absent_ready_at() {
        let st = SnapshotStatus { phase: Phase::Working, ready_at: None };
        let v = serde_json::to_value(&st).unwrap();
        assert!(!v.as_object().unwrap().contains_key("readyAt"));
        let back: SnapshotStatus = serde_json::from_value(v).unwrap();
        assert_eq!(back, st);
    }

    #[test]
    fn volume_replica_spec_and_status_round_trip() {
        let spec = VolumeReplicaSpec { volume: "v".into(), node: "n".into() };
        let back: VolumeReplicaSpec = serde_json::from_value(serde_json::to_value(&spec).unwrap()).unwrap();
        assert_eq!(back, spec);

        let st = VolumeReplicaStatus {
            phase: "Synced".into(),
            branches: std::collections::BTreeMap::from([("main".to_string(), "abc123".to_string())]),
        };
        let v = serde_json::to_value(&st).unwrap();
        assert_eq!(v["phase"], "Synced");
        let back: VolumeReplicaStatus = serde_json::from_value(v).unwrap();
        assert_eq!(back, st);

        let empty = VolumeReplicaStatus { phase: "Syncing".into(), branches: Default::default() };
        let v = serde_json::to_value(&empty).unwrap();
        assert!(!v.as_object().unwrap().contains_key("branches"));
    }

    /// Nothing sets `deny_unknown_fields`, so an object stored before this task's cutover — still
    /// carrying `durable`, `compatibleNodes`, or `lastSyncAt` — keeps parsing after those fields
    /// are dropped from the schema and the struct. The value just goes nowhere: it disappears on
    /// the object's next write and nothing ever reads it again.
    #[test]
    fn dropped_fields_are_tolerated_on_deserialize() {
        let ws_status = serde_json::json!({
            "phase": "running",
            "durable": "abc123",
            "compatibleNodes": ["node-a", "node-b"],
        });
        serde_json::from_value::<WorkspaceStatus>(ws_status).expect("durable/compatibleNodes must still parse");

        let replica_status = serde_json::json!({
            "phase": "Synced",
            "lastSyncAt": "2026-09-01T00:00:00Z",
        });
        serde_json::from_value::<VolumeReplicaStatus>(replica_status).expect("lastSyncAt must still parse");
    }
}
