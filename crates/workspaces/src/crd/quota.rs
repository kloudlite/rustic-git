//! `Quota`, the retired `QuotaRequest`, and `Request` (quota / access / region / other): the
//! per-owner allocation ceilings, their compiled-in defaults, and the one-pending-per-kind ask a
//! superadmin decides.

use super::*;


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
///
/// `cpu` and `memoryGb` are DERIVED from the count dimensions, never picked independently: quota
/// charges the LIMIT (`workspace_cost`, `environment_cost`), so a ceiling must cover
/// `workspaces x PodResources::default()` plus `environments x 4 services x env_unit_resources()`
/// (four being the environment size we plan for) plus one builder per owner at
/// `PodResources::default()` (the hidden `bld-{slug}` environment, whose buildkit service carries
/// its own resources and whose cpu/memory count against the owner while it runs), or the counts
/// are unreachable and cpu refuses first. Change a count or either limit and recompute this, or
/// the dimensions drift apart again.
pub fn default_quota(team: bool) -> QuotaSpec {
    if team {
        QuotaSpec {
            workspaces: 20,
            environments: 8,
            snapshots: 80,
            disk_gb: 400,
            cpu: 148,
            memory_gb: 296,
            regions: Vec::new(),
        }
    } else {
        QuotaSpec {
            workspaces: 5,
            environments: 2,
            snapshots: 20,
            disk_gb: 100,
            cpu: 40,
            memory_gb: 80,
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
