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
//!   the RBAC split that stops a controller editing its own desired state becomes decorative.
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

pub use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;

pub const GROUP: &str = "rustic-git.io";
pub const VERSION: &str = "v1alpha1";
/// `/v1` writes spec under this manager; the controller writes status under the one below. Two
/// distinct managers is what makes a server-side-apply conflict mean something.
pub const FIELD_MANAGER: &str = "rustic-git";
pub const AGENT_FIELD_MANAGER: &str = "rustic-git-agent";
/// Held while a subvolume exists on a node. The object must outlive the delete request until the
/// controller has actually reclaimed the bytes — otherwise the record of what to reclaim is gone
/// before the reclaim happens.
pub const SUBVOLUME_FINALIZER: &str = "rustic-git.io/subvolume";
/// Held while a `SnapshotRequest` may have work in flight.
///
/// Same reason `Volume` has one, and the reason the earlier "a plain delete, no finalizer" was
/// wrong: that is true of a FINISHED request and false of a working one. A delete during
/// `phase: working` leaves a btrfs RO snapshot, a stage file, an in-flight blob upload and a
/// possible `POST /commits` with no object left to record the outcome in — and the Volume's own
/// finalizer does not cover it, because a SnapshotRequest is deliberately not the Volume's child.
pub const SNAPSHOT_FINALIZER: &str = "rustic-git.io/snapshot";

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
    CloneOf { volume: String },
    /// A pushed commit, named by id, fetched from the registry.
    ///
    /// `region` is the region the RECORD names, which is not always the region this node runs in:
    /// a snapshot pushed from the VM region restores onto a k3s node, and its blobs live in the
    /// VM region's container. The API resolves it (it holds the region store and the caller's
    /// authorization); the agent maps it to credentials. Absent means "this node's own region" —
    /// every record written before this field existed.
    RestoreOf {
        volume: String,
        snapshot_id: String,
        /// The registry owner LABEL the source volume lives under — a team slug for a team's
        /// environment, which is not the owner of the object being restored INTO. Absent means
        /// "the same owner", which is every record written before this and every personal restore.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<String>,
    },
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
    /// the destination's own owner — same rule as `VolumeSource::RestoreOf`.
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
    /// Written ONCE by the `/v1` admission path from the owner's `OwnerBinding`. A pod's affinity
    /// is derived from this and never chosen independently — two places allowed to name a node is
    /// two places that can disagree about where the data is.
    pub node_name: String,
    pub region: String,
    pub quota_gb: u64,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lineage_tip: Option<String>,
    // No `lastSnapshot` and no `lastPush`: "the newest snapshot of this volume" is a query over
    // `SnapshotRequest`s by the `rustic-git.io/volume` label. A second controller writing this
    // status object would prune the first one's fields — `patch_status` applies FORCED under one
    // `AGENT_FIELD_MANAGER`, and server-side apply removes fields a manager previously owned and no
    // longer sets, so the Volume reconciler's very next pass would delete whatever the snapshot
    // reconciler had just written.
    /// Human-readable progress for work that outlives one reconcile (a multi-GB send).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
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
    /// A `SnapshotRequest` whose record is in the registry. Never re-run past this.
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
    /// Optional in release 1: an object created before this field existed must still PARSE, or the
    /// controller 422s every legacy Workspace it tries to write. A legacy object is adopted through
    /// its deprecated `spec.volumeRef` instead; Task 11 is what makes this required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<WorkspaceStorage>,
    pub desired_state: DesiredState,
    /// In-place restore, same wish the Environment takes. Written by the API, consumed by this
    /// object's reconciler. Workspaces do not offer it in the UI yet — the field exists so the
    /// owner-only workspace restore can use the one code path rather than growing a second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<RestoreWish>,
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
    /// The package profile actually converged, reported rather than wished for — `spec` carries
    /// the `kloudlite.yaml` the reconciler last saw; this is what building it produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packages: Option<PackagesStatus>,
}

/// What the reconciler last saw and built from the workspace's `kloudlite.yaml`: `observed` and
/// `observed_hash` are the FILE as of the last pass (the hash is the idempotency key — a rebuild
/// is skipped when it still matches), while `profile` is the Nix store path the profile on disk
/// actually resolved to.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PackagesStatus {
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
pub struct EnvironmentStatus {
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub service_status: Vec<ServiceStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

/// Which node an owner's work lands on. One object per `{region, owner}`.
///
/// Watched by the agent on `spec.nodeName`: this object is what makes an owner's per-team
/// namespaces exist on that node.
#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "rustic-git.io",
    version = "v1alpha1",
    kind = "OwnerBinding",
    plural = "ownerbindings",
    shortname = "ob",
    status = "OwnerBindingStatus",
    selectable = ".spec.nodeName",
    printcolumn = r#"{"name":"Owner","type":"string","jsonPath":".spec.owner"}"#,
    printcolumn = r#"{"name":"Node","type":"string","jsonPath":".spec.nodeName"}"#,
    printcolumn = r#"{"name":"Region","type":"string","jsonPath":".spec.region"}"#,
    derive = "PartialEq"
)]
#[serde(rename_all = "camelCase")]
pub struct OwnerBindingSpec {
    pub owner: String,
    pub region: String,
    pub node_name: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OwnerBindingStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
}

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
    group = "rustic-git.io",
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
pub const VOLUME_LABEL: &str = "rustic-git.io/volume";

/// A push, ready to `create`. Created and never patched: a request is immutable and its outcome
/// lives in its own status, so a second push is a second OBJECT rather than a timestamp moving
/// forward on a shared one — which is what the annotation it replaces could not express.
///
/// The finalizer is set at creation because the work can start on the very first reconcile; adding
/// it later leaves a window where a delete during `working` orphans an in-flight `btrfs send`.
pub fn snapshot_request(name: &str, owner: &str, volume: &str, message: Option<String>) -> SnapshotRequest {
    let mut r = SnapshotRequest::new(name, SnapshotRequestSpec { volume: volume.to_string(), message });
    r.metadata.finalizers = Some(vec![SNAPSHOT_FINALIZER.to_string()]);
    r.metadata.labels = Some(std::collections::BTreeMap::from([
        ("rustic-git.io/owner".to_string(), owner.to_string()),
        (VOLUME_LABEL.to_string(), volume.to_string()),
    ]));
    r
}

/// Has the Volume already granted this exact wish?
///
/// Both halves of the pair, and the ONE place that decides it — the Volume's own guard and its
/// parent's gate must never disagree about whether a restore is finished, or one scales services
/// back up while the other still means to swap the disk under them.
pub fn wish_granted(wish: &RestoreWish, restored_to: Option<&str>, restored_at: Option<&str>) -> bool {
    restored_to == Some(wish.snapshot_id.as_str()) && restored_at == Some(wish.requested_at.as_str())
}

/// `{region}-{owner}` lowercased — the RFC-1123 object name for an owner's node binding.
pub fn binding_name(region: &str, owner: &str) -> String {
    format!("{region}-{owner}").to_lowercase()
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
/// whole namespace (see `k8s::attach_policy`).
pub fn ws_namespace(owner: &str, team: &str) -> String {
    let raw = if team.is_empty() || team.eq_ignore_ascii_case(owner) {
        format!("ws-{}", owner.to_lowercase())
    } else {
        format!("ws-{}-{}", team.to_lowercase(), owner.to_lowercase())
    };
    dns_label(&raw)
}

/// A namespace name is an RFC 1123 label: 63 characters at most. Two 39-character handles and
/// the prefix can reach 82, so a long pair is cut and given a hash tail — the tail is what keeps
/// two pairs that share a prefix apart. Deterministic, so the controller and the API agree.
fn dns_label(raw: &str) -> String {
    if raw.len() <= 63 {
        return raw.to_string();
    }
    use sha2::Digest;
    let h = sha2::Sha256::digest(raw.as_bytes());
    let tail: String = h.iter().take(4).map(|b| format!("{b:02x}")).collect();
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
    match id.strip_prefix("env-") {
        Some(rest) => format!("env-{rest}"),
        None => format!("env-{id}"),
    }
}

/// Every CRD this repo owns, for YAML generation and for a startup precondition check.
pub fn all_crds() -> Vec<CustomResourceDefinition> {
    vec![
        Volume::crd(),
        Workspace::crd(),
        Environment::crd(),
        OwnerBinding::crd(),
        SnapshotRequest::crd(),
    ]
}

/// Condition type set once `status.packages` reflects a successful build of the workspace's
/// `kloudlite.yaml` — named here, not in the agent, because it describes a status field this
/// file owns rather than a controller-local fact like `NAMESPACE_READY`.
pub const PACKAGES_READY: &str = "PackagesReady";

/// A standard condition with `observedGeneration` stamped.
///
/// `meta/v1.Condition` rather than a bespoke struct, because it is the shape
/// `kubectl wait --for=condition=Ready` already reads.
pub fn condition(kind: &str, status: bool, reason: &str, message: &str, generation: i64) -> Condition {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_status_carries_packages_and_omits_it_when_unset() {
        let st = WorkspaceStatus::default();
        assert!(!serde_json::to_string(&st).unwrap().contains("packages"));
        let st = WorkspaceStatus {
            packages: Some(PackagesStatus {
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
}
