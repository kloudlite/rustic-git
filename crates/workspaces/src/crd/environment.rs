//! `Environment`: its services, the hidden builder variant (`system = "builder"`), intercepts and
//! the per-service status the web reads (`intercepted_by`, `unreachable_since`).

use super::*;


#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatus {
    pub name: String,
    pub ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The workspace currently intercepting this service, if any — a view of `EnvironmentSpec`'s
    /// own `intercepts`, reported here so a browse of one service shows its own fate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intercepted_by: Option<String>,
    /// Unix seconds at which the intercepting workspace was first observed unreachable, stamped by
    /// the environment's own controller and cleared the moment it is reachable again.
    ///
    /// It exists because a workspace whose NODE died leaves nothing behind to date the outage: no
    /// pod object, and no controller of its own still stamping `Ready=False`. Without a clock the
    /// grace before the real service comes back is either unbounded or measured off some other
    /// object's timestamp, which dates a different event entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unreachable_since: Option<i64>,
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
    /// A workspace's wish to steal one service's traffic. Empty on every environment predating
    /// this field, so `#[serde(default)]` is load-bearing, not decoration.
    #[serde(default)]
    pub intercepts: Vec<Intercept>,
    /// Marks the hidden per-owner builder environment (`BUILDER_SYSTEM`) so the renderer can tell
    /// it apart from an ordinary one — `None` for every environment a person created, which is
    /// every environment before this field existed too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
}


/// Namespace label naming the system an environment serves (`BUILDER_SYSTEM`), written by the
/// agent from `EnvironmentSpec::system` so the pod fence can widen its rules for exactly that
/// namespace and no other. A view of the spec, like every other label here — never authorization.
pub const SYSTEM_LABEL: &str = "kloudlite.io/system";


/// The one recognised value of `EnvironmentSpec.system` today: the per-owner buildkitd
/// environment a workspace image build starts on demand (see Task 1's spike).
pub const BUILDER_SYSTEM: &str = "builder";


/// The builder's own cache subvolume, in GB. A constant for now; `builder_cache_gb` becomes a
/// live setting once anyone needs a different number per region.
pub const BUILDER_CACHE_GB: u64 = 50;


/// `bld-{slug}` — the builder environment's id, deterministic from the owner slug so a build
/// gate can name it without a lookup.
pub fn builder_id(slug: &str) -> String {
    format!("bld-{slug}")
}


/// One workspace's wish to receive an environment service's traffic instead of the service
/// itself.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Intercept {
    pub service: String,
    pub workspace: String,
    #[serde(default)]
    pub ports: Vec<PortMap>,
}


impl Intercept {
    /// The workspace port that answers for `service_port`, or the same number when nothing
    /// names it — an intercept with no `ports` entry still forwards everything 1:1.
    pub fn workspace_port(&self, service_port: u16) -> u16 {
        self.ports
            .iter()
            .find(|p| p.service == service_port)
            .map_or(service_port, |p| p.workspace)
    }
}


/// One port rewrite within an `Intercept`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortMap {
    pub service: u16,
    pub workspace: u16,
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
