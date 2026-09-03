//! What an owner is using, computed from the objects themselves.
//!
//! Never cached, never stored in a status field. A stored counter can only be wrong in one
//! direction that matters — under-counting, which hands out allocation nobody has — and the lists
//! below are already indexed by the owner label, so the truth costs four list calls. The label is
//! the INDEX; every sum re-reads `spec.owner`, because a label is a view and never authorization.

use crate::crd;
use crate::k8s::OWNER_LABEL;
use kube::api::{Api, ListParams};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub workspaces: u32,
    pub environments: u32,
    pub snapshots: u32,
    pub disk_gb: u64,
    pub cpu: u32,
    pub memory_gb: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dim {
    Workspaces,
    Environments,
    Snapshots,
    DiskGb,
    Cpu,
    MemoryGb,
}

impl Dim {
    /// The word the 409 says. It is also the `QuotaSpec` field name on the wire, so the web can
    /// key the request form off the refusal without a second mapping to keep in step.
    pub fn word(self) -> &'static str {
        match self {
            Dim::Workspaces => "workspaces",
            Dim::Environments => "environments",
            Dim::Snapshots => "snapshots",
            Dim::DiskGb => "diskGb",
            Dim::Cpu => "cpu",
            Dim::MemoryGb => "memoryGb",
        }
    }

    fn of(self, q: &crd::QuotaSpec) -> u64 {
        match self {
            Dim::Workspaces => q.workspaces as u64,
            Dim::Environments => q.environments as u64,
            Dim::Snapshots => q.snapshots as u64,
            Dim::DiskGb => q.disk_gb,
            Dim::Cpu => q.cpu as u64,
            Dim::MemoryGb => q.memory_gb as u64,
        }
    }

    fn used(self, u: &Usage) -> u64 {
        match self {
            Dim::Workspaces => u.workspaces as u64,
            Dim::Environments => u.environments as u64,
            Dim::Snapshots => u.snapshots as u64,
            Dim::DiskGb => u.disk_gb,
            Dim::Cpu => u.cpu as u64,
            Dim::MemoryGb => u.memory_gb as u64,
        }
    }
}

/// The exact sentence the design doc specifies. One function, because the web keys off its shape
/// and six call sites formatting their own would drift.
pub fn refuse(dim: Dim, used: u64, limit: u64) -> String {
    format!("{}: {used} of {limit} in use; request more under Quota", dim.word())
}

/// Read-then-write, so two concurrent creates can overshoot by one. Accepted deliberately (design
/// doc §2): the `ResourceQuota` the agent writes is the hard stop for the dimensions where an
/// overshoot costs real capacity, and a lock across the API tier's replicas would cost more than
/// the one object it saves.
pub fn check(dim: Dim, limit: &crd::QuotaSpec, used: &Usage, adding: u64) -> Result<(), String> {
    let (have, cap) = (dim.used(used), dim.of(limit));
    if have + adding > cap {
        return Err(refuse(dim, have, cap));
    }
    Ok(())
}

fn owned_by(owner: &str) -> ListParams {
    ListParams::default().labels(&format!("{OWNER_LABEL}={owner}"))
}

/// A live working copy: the person's desired state is Running. A stopped workspace still holds its
/// disk (counted under `diskGb`) but is not holding cpu or memory on any node, and charging for
/// capacity nobody is occupying is what would make stopping pointless.
fn live(d: crd::DesiredState) -> bool {
    d == crd::DesiredState::Running
}

/// Milli-cores from a Kubernetes cpu quantity. `0` for anything unrecognised: a hand-edited spec
/// must not be a way to look under quota by making the sum unreadable, and it must not panic on a
/// listing every page does.
pub fn millicores(q: &str) -> u64 {
    match q.strip_suffix('m') {
        Some(n) => n.parse().unwrap_or(0),
        None => q.parse::<f64>().map(|v| (v * 1000.0) as u64).unwrap_or(0),
    }
}

/// Mebibytes from a Kubernetes memory quantity, for the two suffixes this repo writes.
pub fn mebibytes(q: &str) -> u64 {
    for (suffix, mib) in [("Gi", 1024u64), ("Mi", 1), ("G", 954), ("M", 1)] {
        if let Some(n) = q.strip_suffix(suffix) {
            return n.parse::<u64>().unwrap_or(0) * mib;
        }
    }
    // Bare bytes.
    q.parse::<u64>().unwrap_or(0) / (1024 * 1024)
}

fn ceil_div(n: u64, d: u64) -> u64 {
    n.div_ceil(d)
}

/// Everything `owner` is using right now. Four list calls, all label-selected.
pub async fn usage(c: &kube::Client, owner: &str) -> Result<Usage, kube::Error> {
    let ws: Api<crd::Workspace> = Api::all(c.clone());
    let envs: Api<crd::Environment> = Api::all(c.clone());
    let vols: Api<crd::Volume> = Api::all(c.clone());
    let snaps: Api<crd::Snapshot> = Api::all(c.clone());
    let lp = owned_by(owner);

    let (mut millis, mut mib) = (0u64, 0u64);
    let mut u = Usage::default();

    for w in ws.list(&lp).await?.items {
        if w.spec.owner != owner {
            continue;
        }
        u.workspaces += 1;
        if live(w.spec.desired_state) {
            millis += millicores(&w.spec.resources.cpu_limit);
            mib += mebibytes(&w.spec.resources.memory_limit);
        }
    }
    for e in envs.list(&lp).await?.items {
        if e.spec.owner != owner {
            continue;
        }
        u.environments += 1;
        if live(e.spec.desired_state) {
            // Every service gets the env unit — one definition, in `k8s::env_unit_resources`, used
            // by the StatefulSet and by the namespace's LimitRange. Reading it here is what keeps
            // the accounting and what actually runs from being two numbers.
            let unit = crate::k8s::env_unit_resources();
            let n = e.spec.services.len() as u64;
            millis += n * millicores(&unit.cpu_limit);
            mib += n * mebibytes(&unit.memory_limit);
        }
    }
    for v in vols.list(&lp).await?.items {
        if v.spec.owner != owner {
            continue;
        }
        // Detached volumes included: disk kept by snapshots after a working copy is deleted is
        // still the owner's disk, and deleting the snapshots is how they get it back.
        u.disk_gb += v.spec.quota_gb;
    }
    for s in snaps.list(&lp).await?.items {
        // `is_snapshot`, not `!spec.transient`: a legacy baseline is a sync point by shape rather
        // than by flag, and the agent's own sync points are never anyone's allocation.
        if s.spec.owner == owner && s.is_snapshot() {
            u.snapshots += 1;
        }
    }
    u.cpu = ceil_div(millis, 1000) as u32;
    u.memory_gb = ceil_div(mib, 1024) as u32;
    Ok(u)
}

/// The owner's own `Quota`, or the default object for their kind, or the compiled-in table.
///
/// Three levels because each missing level is a real state: a new owner has no object, a fresh
/// cluster has no `default-*` object either, and neither may read as "unlimited".
pub async fn effective(c: &kube::Client, owner: &str, team: bool) -> Result<crd::QuotaSpec, kube::Error> {
    let api: Api<crd::Quota> = Api::all(c.clone());
    if let Some(q) = api.get_opt(owner).await? {
        return Ok(q.spec);
    }
    let fallback = if team { crd::DEFAULT_TEAM_QUOTA } else { crd::DEFAULT_USER_QUOTA };
    if let Some(q) = api.get_opt(fallback).await? {
        return Ok(q.spec);
    }
    Ok(crd::default_quota(team))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantities_parse_the_forms_this_repo_writes() {
        // `PodResources::default` and `k8s::env_unit_resources` between them write exactly these.
        assert_eq!(millicores("4"), 4000);
        assert_eq!(millicores("250m"), 250);
        assert_eq!(millicores("2"), 2000);
        assert_eq!(mebibytes("8Gi"), 8192);
        assert_eq!(mebibytes("2730Mi"), 2730);
        assert_eq!(mebibytes("4Gi"), 4096);
        // An unparseable quantity is 0, never a panic and never a silent huge number: a bad value
        // must not be a way to look over quota, and it must not take the whole listing down.
        assert_eq!(millicores("nonsense"), 0);
        assert_eq!(mebibytes(""), 0);
    }

    #[test]
    fn the_refusal_names_the_dimension_the_limit_and_the_use() {
        assert_eq!(refuse(Dim::Workspaces, 5, 5), "workspaces: 5 of 5 in use; request more under Quota");
        assert_eq!(refuse(Dim::DiskGb, 96, 100), "diskGb: 96 of 100 in use; request more under Quota");
        assert_eq!(refuse(Dim::MemoryGb, 32, 32), "memoryGb: 32 of 32 in use; request more under Quota");
    }

    #[test]
    fn a_check_refuses_only_when_the_addition_would_cross_the_limit() {
        let limit = crate::crd::default_quota(false);
        let used = Usage { workspaces: 4, ..Default::default() };
        assert!(check(Dim::Workspaces, &limit, &used, 1).is_ok(), "4 + 1 of 5 fits");
        let used = Usage { workspaces: 5, ..Default::default() };
        let msg = check(Dim::Workspaces, &limit, &used, 1).unwrap_err();
        assert_eq!(msg, "workspaces: 5 of 5 in use; request more under Quota");
    }
}
