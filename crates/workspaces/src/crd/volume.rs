//! `Volume` and `VolumeReplica`: the one btrfs subvolume a workspace or environment owns, its
//! source (fresh, a clone of a sibling, a restore onto a snapshot), which node holds it, and the
//! per-node replica rows that say who else holds a synced copy. Reference-counted through
//! ownerReferences — see the project guide's "Workspaces and environments".

use super::*;


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


/// `{volume}.{node}` — deterministic so two callers naming the same volume/node pair always agree
/// on the one `VolumeReplica` object, rather than racing to create duplicates.
pub fn replica_name(volume: &str, node: &str) -> String {
    format!("{volume}.{node}")
}


/// The label a `Snapshot` carries so `/v1/volumes/{id}/history` is one indexed list
/// call rather than a scan. Same rule as every other label here: a VIEW of `spec.volume`, never
/// authorization.
pub const VOLUME_LABEL: &str = "kloudlite.io/volume";
