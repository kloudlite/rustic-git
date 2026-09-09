//! `Workspace`: spec (image, packages and their locks, resources, storage source, attachment) and
//! the status the node controller writes back, with the condition names both sides share.

use super::*;


/// What the user asked of a parent object's storage. This is what the API used to author directly
/// as a `VolumeSpec`; the parent's reconciler is what turns it into a `Volume` now.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceStorage {
    pub quota_gb: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<VolumeSource>,
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
    /// What each `name@version` entry in `packages` resolved to. Desired state on purpose: the
    /// agent has no internet, so the api resolves once and the answer rides in the object — see
    /// `Lock`. A bare entry (no `@`) has no lock.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locks: Vec<Lock>,
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
    /// What `spec.locks` was when this profile was built — the observed half of the lock, so the
    /// web can show `nodejs@20 -> 20.20.2` without reading spec and guessing whether the node has
    /// caught up with it yet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locked: Vec<LockedStatus>,
}


/// The reported shape of a `Lock`: what the person asked for, what it became, and where from.
/// Deliberately narrower than `Lock` — a store path is a build detail nobody reads off status.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LockedStatus {
    pub entry: String,
    pub version: String,
    pub rev: String,
}


/// Condition type set once `status.packages` reflects a successful build of `spec.packages` —
/// named here, not in the agent, because it describes a status field this file owns rather than
/// a controller-local fact like `NAMESPACE_READY`.
pub const PACKAGES_READY: &str = "PackagesReady";


/// `PackagesReady=False` reason: a lock's store path is not in `cache.nixos.org`. The agent never
/// builds from source, so this is terminal until somebody picks a version that has a binary.
pub const PKG_NOT_CACHED: &str = "NotCached";


/// `PackagesReady=False` reason: an `@` entry arrived with no lock. `/v1` never writes one — this
/// is an operator having edited spec with kubectl, and the agent refuses to guess a version.
pub const PKG_UNRESOLVED: &str = "Unresolved";


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
