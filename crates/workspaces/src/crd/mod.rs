//! The `kloudlite.io/v1alpha1` custom resources — the reconcile substrate for workspaces and
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

mod names;
pub use names::*;

pub const GROUP: &str = "kloudlite.io";
pub const VERSION: &str = "v1alpha1";
/// The controller writes status under its own manager; a server-side-apply conflict against it
/// therefore means another controller, not `/v1`.
pub const AGENT_FIELD_MANAGER: &str = "kloudlite-agent";
/// The admin process's own field manager on the settings routes — distinct from
/// `AGENT_FIELD_MANAGER` so a settings write and the agent's own status writes are never
/// attributed to the same manager in a server-side-apply conflict.
pub const AGENT_FIELD_MANAGER_ADMIN: &str = "kloudlite-admin";
/// `ClusterSettings`' history annotation: the previous ten specs, newest first, JSON — parallel to
/// `StoredCentralSettings.history` but as an annotation rather than a struct field, since the CRD
/// spec is what server-side apply owns field-by-field and a growing history array there would be a
/// moving target for every other writer of the spec (there are none today, but the annotation
/// keeps the spec itself exactly the shape `ClusterSettingsSpec` declares).
pub const SETTINGS_HISTORY_ANNOTATION: &str = "kloudlite.io/settings-history";
pub const SETTINGS_UPDATED_BY_ANNOTATION: &str = "kloudlite.io/updated-by";
pub const SETTINGS_UPDATED_AT_ANNOTATION: &str = "kloudlite.io/updated-at";
/// Held while a subvolume exists on a node. The object must outlive the delete request until the
/// controller has actually reclaimed the bytes — otherwise the record of what to reclaim is gone
/// before the reclaim happens.
pub const SUBVOLUME_FINALIZER: &str = "kloudlite.io/subvolume";
/// Held on a shared-volume clone workspace. A workspace that is a
/// shared-volume clone (`spec.storage.source` is `CloneOf { commit: Some(_), .. }`) checks out a
/// worktree under the SOURCE volume's `live/`, not its own — it owns no `Volume` child, so
/// nothing's ownerReference GC ever reclaims that worktree. This finalizer is what makes the
/// delete drop it. An owned-volume workspace also carries this finalizer (added uniformly to
/// avoid distinguishing the two cases before the spec's `source` is known to be gone at delete
/// time), but its cleanup is a no-op: the owned `Volume`'s own `SUBVOLUME_FINALIZER` already
/// deletes the whole voldir, worktree included.
pub const WORKTREE_FINALIZER: &str = "kloudlite.io/worktree";

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
    // `Snapshot` CRs by the `kloudlite.io/volume` label. A second controller writing this
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
    group = "kloudlite.io",
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
    group = "kloudlite.io",
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
    /// alone does not say which one to snapshot; the creator (`/push`) names it. The
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
            tracing::warn!(error = %e, value = %v, "snapshot.state.invalid");
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
    group = "kloudlite.io",
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
    kloudlite_core::hex(&b)
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
    group = "kloudlite.io",
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
    group = "kloudlite.io",
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
    group = "kloudlite.io",
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
    /// Whether `spec.owner` names a team rather than a person — a FACT the binding reconciler
    /// derives (no personal Workspace anywhere carries this owner's handle, see
    /// `binding::is_team_owner`), not a directory lookup the agent cannot make. Readers with no
    /// directory of their own (`controller/environment.rs`'s quota sizing) read this instead of
    /// guessing; defaults false because a binding not yet reconciled is more often a person's
    /// first workspace than a team's first environment.
    #[serde(default)]
    pub team: bool,
}

/// What ONE owner — a person or a team slug — may allocate. Cluster-scoped, named by the owner
/// slug, written only by a superadmin through `/v1`.
///
/// Two `default-*` objects are the fallback for an owner with no object of their own, because a
/// slug does not say which it is: `/v1` knows (a team slug is one the directory answers for) and
/// picks. Nothing here is a count of what EXISTS — usage is computed from the objects themselves
/// on every request (`quota::usage`), so no field of this object can drift from the truth.
#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "kloudlite.io",
    version = "v1alpha1",
    kind = "Quota",
    plural = "quotas",
    shortname = "qta",
    status = "QuotaStatus",
    printcolumn = r#"{"name":"Workspaces","type":"integer","jsonPath":".spec.workspaces"}"#,
    printcolumn = r#"{"name":"Environments","type":"integer","jsonPath":".spec.environments"}"#,
    printcolumn = r#"{"name":"DiskGb","type":"integer","jsonPath":".spec.diskGb"}"#,
    derive = "PartialEq"
)]
#[serde(rename_all = "camelCase")]
pub struct QuotaSpec {
    /// Live working copies of kind Workspace.
    pub workspaces: u32,
    /// Live working copies of kind Environment.
    pub environments: u32,
    /// Snapshots — pushes, not sync points. The agent's own transient cuts are its business and
    /// are never anyone's allocation.
    pub snapshots: u32,
    /// Sum of `Volume.spec.quotaGb` over every volume of this owner, DETACHED INCLUDED: disk kept
    /// by snapshots after a working copy is deleted is still the owner's disk.
    pub disk_gb: u64,
    /// Whole cores, summed over live working copies' limits.
    pub cpu: u32,
    pub memory_gb: u32,
    /// Regions this owner has been GRANTED beyond whatever placement offers by default. Recorded
    /// here, on the one per-owner cluster-scoped object the admin process already owns, rather
    /// than on an `OwnerBinding` — a binding is per `{owner, region}` and is authored by the
    /// claiming agent, so a per-owner grant list has no coherent home there. Nothing reads it for
    /// placement yet (spec §B: "a recorded decision only"); per-owner region gating lands later
    /// and reads exactly this field.
    ///
    /// Skipped when empty on purpose: `write_quota` merge-patches a whole `QuotaSpec`, and
    /// `PUT /admin/quota/{owner}` bodies never mention regions — serializing `[]` would erase a
    /// grant every time somebody edited a limit.
    ///
    /// ponytail: that same skip makes a grant a ONE-WAY DOOR — a merge patch can add to this list
    /// and never remove from it, so there is no revoke path short of editing the `Quota` by hand.
    /// Acceptable while nothing reads the field for placement; the day it gates anything, revoke
    /// becomes a route of its own that sends the full list (a JSON patch on `/spec/regions`, not a
    /// merge patch) rather than another field on the quota body.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regions: Vec<String>,
}

/// Nothing writes this today. It exists because every CRD in this repo has a status subresource —
/// without one a status write folds into spec and the RBAC spec/status split becomes decorative —
/// and `crd_yaml.rs` enforces that for every kind.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct QuotaStatus {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

pub const DEFAULT_USER_QUOTA: &str = "default-user";
pub const DEFAULT_TEAM_QUOTA: &str = "default-team";

/// The bootstrap table from the design doc, owner-approved 2026-09-03. In code rather than in a
/// manifest so an owner with no `Quota` and a cluster with no `default-*` object still has a
/// definite ceiling — a missing fallback object must not mean "unlimited".
pub fn default_quota(team: bool) -> QuotaSpec {
    if team {
        QuotaSpec {
            workspaces: 20,
            environments: 8,
            snapshots: 80,
            disk_gb: 400,
            cpu: 32,
            memory_gb: 128,
            regions: Vec::new(),
        }
    } else {
        QuotaSpec {
            workspaces: 5,
            environments: 2,
            snapshots: 20,
            disk_gb: 100,
            cpu: 8,
            memory_gb: 32,
            regions: Vec::new(),
        }
    }
}

/// The six fields again, every one optional: a request raises the dimensions it names and says
/// nothing about the rest, so approving it must not silently reset a limit somebody already
/// granted.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestedQuota {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspaces: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environments: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshots: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_gb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_gb: Option<u32>,
}

/// A person asking for more, and the decision on it.
///
/// The one kind whose STATUS `/v1` writes rather than a controller: no controller reconciles a
/// request — a person decides it — so the decision has nowhere else to live. Requests are never
/// deleted by the system; the record of who asked for what, and who said yes, is the point.
#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "kloudlite.io",
    version = "v1alpha1",
    kind = "QuotaRequest",
    plural = "quotarequests",
    shortname = "qreq",
    status = "QuotaRequestStatus",
    printcolumn = r#"{"name":"Owner","type":"string","jsonPath":".spec.owner"}"#,
    printcolumn = r#"{"name":"State","type":"string","jsonPath":".status.state"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#,
    derive = "PartialEq"
)]
#[serde(rename_all = "camelCase")]
pub struct QuotaRequestSpec {
    pub owner: String,
    pub requested: RequestedQuota,
    #[serde(default)]
    pub reason: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct QuotaRequestStatus {
    pub state: RequestState,
    /// The deciding superadmin's email, for the audit trail. Never an owner of anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// An enum, not a string, so the API server refuses a typo with a 422 — the same reason `Phase` is
/// one. A request with no status at all is pending: `/v1` creates the object and patches status
/// separately, and the window between the two must not read as "decided".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum RequestState {
    #[default]
    Pending,
    Approved,
    Denied,
}

/// What a person is asking for. One CRD for all four kinds, because the LIFECYCLE is identical —
/// opened by a user, one pending at a time, decided by a superadmin, kept forever as the record —
/// and only the payload and what approve DOES differ. Four CRDs would have meant four RBAC rules,
/// four list routes and four console tables for one workflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum RequestKind {
    Quota,
    Access,
    Region,
    Other,
}

impl RequestKind {
    /// The wire word, for a filter query and for the audit target — one spelling, so a URL's
    /// `?kind=` and a stored object can never disagree.
    pub fn as_str(&self) -> &'static str {
        match self {
            RequestKind::Quota => "quota",
            RequestKind::Access => "access",
            RequestKind::Region => "region",
            RequestKind::Other => "other",
        }
    }
}

/// Join a team, or move to a different role in one. `role` is the directory's own word
/// (`member` / `admin` / `owner`) rather than an enum, because the directory's `Role` lives in
/// `kloudlite-pulls` and this crate deliberately does not depend on it; `validate` is what stops
/// a typo reaching the grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccessAsk {
    pub team: String,
    pub role: String,
}

pub const ROLES: [&str; 3] = ["member", "admin", "owner"];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RegionAsk {
    pub region: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OtherAsk {
    pub title: String,
    pub body: String,
}

/// A person asking for something, and the decision on it. Supersedes `QuotaRequest`, which stays
/// readable until the one-shot migration has run everywhere and a later release retires it.
///
/// Like `QuotaRequest`, this is the one shape whose STATUS the API tier writes rather than a
/// controller: no controller reconciles a request — a person decides it — so the decision has
/// nowhere else to live.
#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "kloudlite.io",
    version = "v1alpha1",
    kind = "Request",
    plural = "requests",
    shortname = "req",
    status = "RequestStatus",
    printcolumn = r#"{"name":"Owner","type":"string","jsonPath":".spec.owner"}"#,
    printcolumn = r#"{"name":"Kind","type":"string","jsonPath":".spec.kind"}"#,
    printcolumn = r#"{"name":"State","type":"string","jsonPath":".status.state"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#,
    derive = "PartialEq"
)]
#[serde(rename_all = "camelCase")]
pub struct RequestSpec {
    /// The person or team the request is FOR — truth, never a label. For an access request this
    /// is the asker's own slug and `access.team` names the team they want into: the team is what
    /// they do not have yet, so it cannot also be the owner that authorizes the ask.
    pub owner: String,
    pub kind: RequestKind,
    /// The signed-in user who opened it. Set by `/v1` from the caller's claims, never from the
    /// body — a request that could name its own author is not evidence of anything.
    pub requested_by: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<RequestedQuota>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<AccessAsk>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<RegionAsk>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<OtherAsk>,
}

impl RequestSpec {
    /// Exactly the block for its kind, and nothing else. `approve` dispatches on `kind`, so a
    /// request carrying a second block would have a payload the decision silently ignores — and
    /// a request carrying none would be approved into a no-op.
    pub fn validate(&self) -> Result<(), String> {
        let present = [
            ("quota", self.quota.is_some()),
            ("access", self.access.is_some()),
            ("region", self.region.is_some()),
            ("other", self.other.is_some()),
        ];
        let want = self.kind.as_str();
        if !present.iter().any(|(name, is_set)| *is_set && *name == want) {
            return Err(format!("kind {want} needs a {want} block"));
        }
        for (name, is_set) in present {
            if is_set && name != want {
                return Err(format!("only the {want} block belongs on a {want} request"));
            }
        }
        if let Some(a) = &self.access {
            if !ROLES.contains(&a.role.as_str()) {
                return Err("role must be member, admin or owner".to_string());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestStatus {
    pub state: RequestState,
    /// The deciding superadmin's email, for the audit trail. Never an owner of anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// What approve actually DID, in one sentence — the quota that was written, the role that was
    /// set, the recorded region grant, or the free text a superadmin typed for an `other`. Kept
    /// separately from `note` because the note is the decider's message to the asker and this is
    /// the record of the effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
}

/// The label a `Snapshot` carries so `/v1/volumes/{id}/history` is one indexed list
/// call rather than a scan. Same rule as every other label here: a VIEW of `spec.volume`, never
/// authorization.
pub const VOLUME_LABEL: &str = "kloudlite.io/volume";

/// Set on a `Node` by an operator (`kubectl label node <n> kloudlite.io/decommission=true`) to
/// retire it. A LABEL and not an annotation because it is a selector-worthy fact about the node,
/// and because removing it is the documented abort. Only the exact value `"true"` counts: a
/// half-typed label must never drain a node.
pub const DECOMMISSION_LABEL: &str = "kloudlite.io/decommission";

/// The drain's one progress window, written by the draining node's own agent and read by the
/// admin console's decommission gate. Lives here, next to the label, so the tier that WRITES it
/// and the tier that READS it can never spell it differently.
pub const DECOMMISSION_STATUS: &str = "kloudlite.io/decommission-status";

/// The sticky stamp `DECOMMISSION_STATUS` carries once a node holds nothing — the prefix the
/// console gates decommission on.
pub const DRAINED_PREFIX: &str = "drained ";

/// Labels every `Snapshot`/`VolumeReplica` create site stamps: `spec.volume`/`spec.owner` restated
/// as labels so a watch or a list (the e2e's `-l kloudlite.io/volume=...`, `/v1`'s own reads) can
/// select on them — a label cannot be queried out of an arbitrary spec field. A VIEW, same rule as
/// every other label in this file: `spec` stays the truth, this is never read for authorization.
pub fn snapshot_labels(owner: &str, volume: &str) -> std::collections::BTreeMap<String, String> {
    std::collections::BTreeMap::from([
        ("kloudlite.io/owner".to_string(), owner.to_string()),
        (VOLUME_LABEL.to_string(), volume.to_string()),
    ])
}

/// Names the Environment a `stop-{env}` request belongs to, so the environments controller can
/// watch only those instead of every push in the cluster. Also a view: the ownerReference is the
/// link the mapper reads, and this label exists only because a watch cannot select on one.
pub const STOP_LABEL: &str = "kloudlite.io/stop-of";

/// Has the Volume already granted this exact wish?
///
/// Both halves of the pair, and the ONE place that decides it — the Volume's own guard and its
/// parent's gate must never disagree about whether a restore is finished, or one scales services
/// back up while the other still means to swap the disk under them.
pub fn wish_granted(wish: &RestoreWish, restored_to: Option<&str>, restored_at: Option<&str>) -> bool {
    restored_to == Some(wish.snapshot_id.as_str()) && restored_at == Some(wish.requested_at.as_str())
}

/// Built-in defaults for `ClusterSettingsSpec` fields, one `fn` per field so `#[serde(default =
/// "...")]` has a path to name — kept separate from the values themselves so
/// `Settings::from_env` (Task 2) can call the same functions as the env-var fallback, and the CRD
/// default and the env default can never drift apart.
pub mod defaults {
    pub fn sync_secs() -> u64 {
        60
    }
    pub fn replica_secs() -> u64 {
        300
    }
    pub fn decommission_secs() -> u64 {
        30
    }
    pub fn node_dead_secs() -> u64 {
        180
    }
    pub fn peer_send_timeout_secs() -> u64 {
        3600
    }
    pub fn peer_serve_timeout_secs() -> u64 {
        900
    }
    pub fn peer_receive_slack() -> u64 {
        3
    }
    pub fn stop_flush_timeout_secs() -> u64 {
        30
    }
    pub fn nix_timeout_secs() -> u64 {
        1200
    }
    /// Mirrors `bins/agent/src/nix.rs`'s `DEFAULT_BASE_PACKAGES` — duplicated, not imported,
    /// because `crates/workspaces` cannot depend on `bins/agent` (the dependency runs the other
    /// way). Keep the two strings in sync by hand; a mismatch is silent, not a compile error.
    pub fn base_packages() -> String {
        "bashInteractive zsh fish starship coreutils git openssh curl less which gnugrep gnused findutils".to_string()
    }
    pub fn default_replicas() -> u32 {
        crate::crd::DEFAULT_REPLICAS
    }
    pub fn max_per_owner() -> u32 {
        50
    }
    pub fn home_cache_gb() -> u32 {
        20
    }
    pub fn quota_gb_ceiling() -> u32 {
        500
    }
    pub fn git_init_image() -> String {
        // Matches the agent's own pre-settings fallback (`bins/agent/src/controller/mod.rs`) —
        // this is a required init container image, not an optional one, so the built-in default
        // cannot be empty the way an unset `runtime_class` legitimately is.
        "alpine/git:2.45.2".to_string()
    }
}

/// One per region, named `default` — the cluster-scoped tunables every agent in that cluster
/// reads on its refresh beat. `spec` is desired (admin-written); `status.observedGeneration`
/// is the last generation an agent actually applied, so the UI's "pending" marker has
/// something to compare against. Cluster-scoped like every other kind here: there is one
/// object per region's k3s, not per namespace.
#[derive(CustomResource, Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "kloudlite.io",
    version = "v1alpha1",
    kind = "ClusterSettings",
    plural = "clustersettings",
    status = "ClusterSettingsStatus"
    // selectable = "": deliberately no selectableFields — agents watch the single `default`
    // object by name, not by node, so there is no per-node axis to select on.
)]
#[serde(rename_all = "camelCase")]
pub struct ClusterSettingsSpec {
    /// Sync-point cut beat interval. 10..=3600 seconds. `None` = admin never set it — the reader
    /// falls back to env, then the built-in default (`AgentSettings::merged_with`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_secs: Option<u64>,
    /// Replication pull beat interval. 30..=3600 seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replica_secs: Option<u64>,
    /// Decommission-beat interval. 5..=600 seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decommission_secs: Option<u64>,
    /// How long a node must be observed NotReady before it is declared dead for placement.
    /// 60..=3600 seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_dead_secs: Option<u64>,
    /// `btrfs send`-over-HTTP client timeout. 60..=21600 seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_send_timeout_secs: Option<u64>,
    /// The send side's own deadline, deliberately shorter than the client's. 60..=21600 seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_serve_timeout_secs: Option<u64>,
    /// Slack added to the receive-side timeout over the serve-side one. 0..=60 seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_receive_slack: Option<u64>,
    /// Deadline for a stop's flush before the pod is torn down anyway. 5..=300 seconds.
    // ponytail: no caller reads this yet; ships for the admin UI ahead of the enforcement it
    // is meant for. Add the read when a stop-flush deadline is actually implemented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_flush_timeout_secs: Option<u64>,
    /// Nix build timeout. 60..=7200 seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nix_timeout_secs: Option<u64>,
    /// Nixpkgs revision pin (`github:NixOS/nixpkgs/<rev>`). `None` = whatever the agent's own
    /// env default is — this field does not carry a built-in default of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nixpkgs: Option<String>,
    /// Packages prepended to every workspace's profile, space-separated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_packages: Option<String>,
    /// Default `Volume.spec.replicas` for a newly created volume. 1..=5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_replicas: Option<u32>,
    /// Max workspaces+environments per owner in this region, until `Quota` fully replaces it.
    /// 1..=1000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_per_owner: Option<u32>,
    /// Home-cache local subvolume quota per (owner, node). 1..=500 GiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_cache_gb: Option<u32>,
    /// Ceiling `clamp_quota` enforces on a requested quota. 10..=5000 GiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_gb_ceiling: Option<u32>,
    /// Tenant workspace pod image. **Boot** — the agent reads this at pod-template render
    /// time, not per reconcile; a change rolls `kloudlite-agent` (Task 5). `None` = keep
    /// today's env value, so an admin who never opens this row cannot blank a required image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_image: Option<String>,
    /// The init container that clones a workspace's seed repo over SSH. **Boot**, same reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_init_image: Option<String>,
    /// k8s `runtimeClassName` for tenant pods (e.g. `gvisor`); `None` = host kernel. **Boot**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_class: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClusterSettingsStatus {
    /// The generation an agent last successfully applied. Compared against
    /// `metadata.generation` by the admin UI's pending marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
}

/// Which mechanism carries each `ClusterSettingsSpec` field: `Live` (next refresh beat picks it
/// up) or `Boot` (only a pod-template rebuild reads it, so it needs a roll of the readers named
/// here). A test (`cluster_setting_meta_is_exhaustive`) asserts this table's field names equal
/// `ClusterSettingsSpec`'s schemars property names, so a field added to the struct without an
/// entry here fails loudly instead of shipping unreadable.
pub const CLUSTER_SETTING_META: &[(&str, kloudlite_core::settings::Mark, &[&str])] = &[
    ("syncSecs", kloudlite_core::settings::Mark::Live, &[]),
    ("replicaSecs", kloudlite_core::settings::Mark::Live, &[]),
    ("decommissionSecs", kloudlite_core::settings::Mark::Live, &[]),
    ("nodeDeadSecs", kloudlite_core::settings::Mark::Live, &[]),
    ("peerSendTimeoutSecs", kloudlite_core::settings::Mark::Live, &[]),
    ("peerServeTimeoutSecs", kloudlite_core::settings::Mark::Live, &[]),
    ("peerReceiveSlack", kloudlite_core::settings::Mark::Live, &[]),
    ("stopFlushTimeoutSecs", kloudlite_core::settings::Mark::Live, &[]),
    ("nixTimeoutSecs", kloudlite_core::settings::Mark::Live, &[]),
    ("nixpkgs", kloudlite_core::settings::Mark::Live, &[]),
    ("basePackages", kloudlite_core::settings::Mark::Live, &[]),
    ("defaultReplicas", kloudlite_core::settings::Mark::Live, &[]),
    ("maxPerOwner", kloudlite_core::settings::Mark::Live, &[]),
    ("homeCacheGb", kloudlite_core::settings::Mark::Live, &[]),
    ("quotaGbCeiling", kloudlite_core::settings::Mark::Live, &[]),
    ("defaultImage", kloudlite_core::settings::Mark::Boot, &["kloudlite-agent"]),
    ("gitInitImage", kloudlite_core::settings::Mark::Boot, &["kloudlite-agent"]),
    ("runtimeClass", kloudlite_core::settings::Mark::Boot, &["kloudlite-agent"]),
];

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
        Quota::crd(),
        QuotaRequest::crd(),
        Request::crd(),
        ClusterSettings::crd(),
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
pub const SYNCED_GENERATION: &str = "kloudlite.io/synced-generation";

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
                "apiVersion": "kloudlite.io/v1alpha1", "kind": "Snapshot",
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
    /// are dropped from the schema and the struct entirely (`compatible_nodes` is no longer a
    /// tolerated field either — it is gone from `WorkspaceStatus`/`EnvironmentStatus`, same as
    /// `durable`). The value just goes nowhere: it disappears on the object's next write and
    /// nothing ever reads it again.
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

    /// `CLUSTER_SETTING_META` names every field the admin write path/UI must know about a
    /// `Live`/`Boot` split for. A field added to the struct without a matching entry here would
    /// silently ship with no mark — meaning no reader ever gets told to roll.
    #[test]
    fn cluster_setting_meta_is_exhaustive() {
        use kube::CustomResourceExt;
        let crd = ClusterSettings::crd();
        let schema = crd.spec.versions[0].schema.as_ref().unwrap().open_api_v3_schema.as_ref().unwrap();
        let mut props: Vec<&str> =
            schema.properties.as_ref().unwrap()["spec"].properties.as_ref().unwrap().keys().map(|k| k.as_str()).collect();
        props.sort_unstable();
        let mut meta: Vec<&str> = CLUSTER_SETTING_META.iter().map(|(name, _, _)| *name).collect();
        meta.sort_unstable();
        assert_eq!(props, meta, "CLUSTER_SETTING_META must name exactly ClusterSettingsSpec's fields");
    }
}

#[cfg(test)]
mod request_tests {
    use super::*;

    fn base(kind: RequestKind) -> RequestSpec {
        RequestSpec {
            owner: "acme".into(),
            kind,
            requested_by: "meera".into(),
            reason: "more room".into(),
            quota: None,
            access: None,
            region: None,
            other: None,
        }
    }

    /// The wire form is what an operator reads with `kubectl get request -o yaml`, and what a
    /// stored object parses back from — both directions, so a rename cannot pass unnoticed.
    #[test]
    fn a_request_round_trips_through_its_wire_form() {
        let mut spec = base(RequestKind::Access);
        spec.access = Some(AccessAsk { team: "acme".into(), role: "admin".into() });
        let v = serde_json::to_value(&spec).unwrap();
        assert_eq!(v["kind"], "access");
        assert_eq!(v["requestedBy"], "meera");
        assert_eq!(v["access"]["role"], "admin");
        // Blocks for the other three kinds are absent, not null: a null would advertise a field
        // the request never carried.
        assert!(v.get("quota").is_none() && v.get("region").is_none() && v.get("other").is_none());
        assert_eq!(serde_json::from_value::<RequestSpec>(v).unwrap(), spec);
    }

    /// A request carrying somebody else's block is not a typo to tolerate: `approve` dispatches on
    /// `kind` and would silently ignore the block that was actually filled in.
    #[test]
    fn exactly_the_block_for_its_kind_must_be_present() {
        let mut ok = base(RequestKind::Quota);
        ok.quota = Some(RequestedQuota { workspaces: Some(9), ..Default::default() });
        assert_eq!(ok.validate(), Ok(()));

        let missing = base(RequestKind::Quota);
        assert_eq!(missing.validate(), Err("kind quota needs a quota block".to_string()));

        let mut extra = base(RequestKind::Quota);
        extra.quota = Some(RequestedQuota::default());
        extra.other = Some(OtherAsk { title: "t".into(), body: "b".into() });
        assert_eq!(extra.validate(), Err("only the quota block belongs on a quota request".to_string()));

        let mut wrong = base(RequestKind::Region);
        wrong.access = Some(AccessAsk { team: "acme".into(), role: "admin".into() });
        assert_eq!(wrong.validate(), Err("kind region needs a region block".to_string()));
    }

    /// Only the three directory roles; anything else would reach `grant_access` as a role nothing
    /// can map, and a 500 on approve is a decision that half-happened.
    #[test]
    fn an_access_request_takes_only_a_real_role() {
        let mut spec = base(RequestKind::Access);
        spec.access = Some(AccessAsk { team: "acme".into(), role: "superuser".into() });
        assert_eq!(spec.validate(), Err("role must be member, admin or owner".to_string()));
    }

    /// `regions` is a granted list, and an empty one is omitted so a merge patch of a `QuotaSpec`
    /// that never mentions regions (every `PUT /admin/quota/{owner}` body) cannot erase a grant.
    #[test]
    fn an_empty_region_grant_is_omitted_from_a_quota_patch() {
        let v = serde_json::to_value(default_quota(false)).unwrap();
        assert!(v.get("regions").is_none());
    }
}
