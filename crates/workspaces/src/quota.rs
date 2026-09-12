//! What an owner is using, computed from the objects themselves.
//!
//! Never cached, never stored in a status field. A stored counter can only be wrong in one
//! direction that matters — under-counting, which hands out allocation nobody has — and the lists
//! below are already indexed by the owner label, so the truth costs four list calls. The label is
//! the INDEX; every sum re-reads `spec.owner`, because a label is a view and never authorization.

use crate::crd;
use crate::crd::VOLUME_LABEL;
use crate::k8s::{OWNER_LABEL, TEAM_LABEL};
use std::collections::HashSet;
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
    // `Ki` first and by division: a Node's `status.allocatable` memory is written that way
    // (`131923924Ki`), and the claim's capacity check reads it through here.
    if let Some(n) = q.strip_suffix("Ki") {
        return n.parse::<u64>().unwrap_or(0) / 1024;
    }
    for (suffix, mib) in [("Gi", 1024u64), ("Mi", 1), ("G", 954), ("M", 1)] {
        if let Some(n) = q.strip_suffix(suffix) {
            return n.parse::<u64>().unwrap_or(0) * mib;
        }
    }
    // Bare bytes.
    q.parse::<u64>().unwrap_or(0) / (1024 * 1024)
}

/// Everything `owner` is using right now — label-selected listings, decided on spec.
///
/// Who an object is CHARGED to: a team environment is stamped `spec.owner = team`, but a team
/// workspace (and its volume) is stamped with the person who made it plus `spec.team = team` —
/// the home and the keys stay the person's. So a team's count comes through the team label and
/// the person's count skips what the team is charged for, and a snapshot is charged to whoever
/// its volume is charged to. Until 2026-09-12 only `spec.owner` was summed, which meant a team's
/// workspaces, disk and snapshots were never counted against anyone at all.
pub async fn usage(c: &kube::Client, owner: &str) -> Result<Usage, kube::Error> {
    use kube::ResourceExt;
    let ws: Api<crd::Workspace> = Api::all(c.clone());
    let envs: Api<crd::Environment> = Api::all(c.clone());
    let vols: Api<crd::Volume> = Api::all(c.clone());
    let snaps: Api<crd::Snapshot> = Api::all(c.clone());
    let lp = owned_by(owner);
    let team_lp = ListParams::default().labels(&format!("{TEAM_LABEL}={owner}"));
    let charged = |o: &str, team: &str| if team.is_empty() { o == owner } else { team == owner };

    // The six listings are independent reads; serially awaited they were six round trips on a
    // path every create and every quota page runs (2026-09-12). Concurrency only — the counts are
    // still recomputed from the CRDs on every request, never cached.
    let (ws_own, ws_team, env_own, vol_own, vol_team, snap_own) = futures::try_join!(
        ws.list(&lp),
        ws.list(&team_lp),
        envs.list(&lp),
        vols.list(&lp),
        vols.list(&team_lp),
        snaps.list(&lp),
    )?;

    let (mut millis, mut mib) = (0u64, 0u64);
    let mut u = Usage::default();
    // Two selectors can answer the same object (a fake that ignores selectors, a label healed
    // late); the name decides once.
    let mut seen: HashSet<String> = HashSet::new();

    for w in ws_own.items.into_iter().chain(ws_team.items) {
        if !charged(&w.spec.owner, &w.spec.team) || !seen.insert(format!("ws/{}", w.name_any())) {
            continue;
        }
        u.workspaces += 1;
        if live(w.spec.desired_state) {
            millis += millicores(&w.spec.resources.cpu_limit);
            mib += mebibytes(&w.spec.resources.memory_limit);
        }
    }
    for e in env_own.items {
        if e.spec.owner != owner {
            continue;
        }
        // The hidden per-owner builder is not one of the owner's environments — they never made
        // it, cannot see it, and must not have their `environments` ceiling spent by it. Its DISK
        // and, while it runs, its capacity are still theirs, which is why only the count skips.
        if e.spec.system.is_none() {
            u.environments += 1;
        }
        if live(e.spec.desired_state) {
            // Every service gets the env unit — one definition, in `k8s::env_unit_resources`, used
            // by the StatefulSet and by the namespace's LimitRange — unless the service names its
            // own (the builder does). Reading what the StatefulSet reads is what keeps the
            // accounting and what actually runs from being two numbers.
            for svc in &e.spec.services {
                let unit = svc.resources.clone().unwrap_or_else(crate::k8s::env_unit_resources);
                millis += millicores(&unit.cpu_limit);
                mib += mebibytes(&unit.memory_limit);
            }
        }
    }
    // Detached volumes included: disk kept by snapshots after a working copy is deleted is still
    // the owner's disk, and deleting the snapshots is how they get it back.
    let mut charged_volumes: Vec<String> = Vec::new();
    let mut known_volumes: HashSet<String> = HashSet::new();
    for v in vol_own.items.into_iter().chain(vol_team.items) {
        known_volumes.insert(v.name_any());
        if !charged(&v.spec.owner, &v.spec.team) || !seen.insert(format!("vol/{}", v.name_any())) {
            continue;
        }
        u.disk_gb += v.spec.quota_gb;
        charged_volumes.push(v.name_any());
    }
    // A snapshot follows its volume; one whose volume is in no listing at all (a fixture, a volume
    // mid-collection) falls back to its own `spec.owner`, the rule this had before teams. A volume
    // that IS listed and charged to someone else takes its snapshots with it.
    let mut count_snapshot = |s: &crd::Snapshot| {
        // `is_snapshot`, not `!spec.transient`: a legacy baseline is a sync point by shape rather
        // than by flag, and the agent's own sync points are never anyone's allocation.
        if s.is_snapshot() && seen.insert(format!("snap/{}", s.name_any())) {
            u.snapshots += 1;
        }
    };
    for s in snap_own.items {
        if s.spec.owner == owner && !known_volumes.contains(&s.spec.volume) {
            count_snapshot(&s);
        }
    }
    for chunk in charged_volumes.chunks(40) {
        let sel = ListParams::default().labels(&format!("{VOLUME_LABEL} in ({})", chunk.join(",")));
        for s in snaps.list(&sel).await?.items {
            if chunk.contains(&s.spec.volume) {
                count_snapshot(&s);
            }
        }
    }
    u.cpu = millis.div_ceil(1000) as u32;
    u.memory_gb = mib.div_ceil(1024) as u32;
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
    // The two default names ARE their kind, regardless of what the directory says about them —
    // `default-team` is not a real team slug, so `Directory::is_team` answers "no" and a caller
    // that trusted it would hand the person table to the team defaults page. Override, don't add
    // a second caller-side check: every caller of `effective` gets this for free.
    let team = match owner {
        crd::DEFAULT_TEAM_QUOTA => true,
        crd::DEFAULT_USER_QUOTA => false,
        _ => team,
    };
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
        // What a Node's allocatable is written in.
        assert_eq!(mebibytes("131923924Ki"), 128831);
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
    fn default_quota_names_are_their_own_kind() {
        // `default_quota(true/false)` is the compiled-in table used when neither the owner's own
        // `Quota` nor a `default-*` object exists in the cluster — exactly the no-kube-client path
        // `effective` falls through to, since these two names never have an owner-named `Quota`.
        assert_eq!(crd::default_quota(true).workspaces, 20);
        assert_eq!(crd::default_quota(false).workspaces, 5);
    }

    /// cpu and memoryGb are derived from the counts, so the counts must actually be reachable:
    /// every workspace plus every environment at the size we plan for has to fit under them. This
    /// is the test that catches a raised count, or a changed pod limit, left out of the table.
    #[test]
    fn the_default_cpu_and_memory_cover_the_counts_they_promise() {
        const SERVICES_PER_ENV: u64 = 4;
        for team in [false, true] {
            let q = crd::default_quota(team);
            let ws = crate::api::workspace_cost(0, &crd::PodResources::default());
            let env = crate::api::environment_cost(0, SERVICES_PER_ENV as usize);
            for dim in [Dim::Cpu, Dim::MemoryGb] {
                let of = |cost: &[(Dim, u64)]| cost.iter().find(|(d, _)| *d == dim).map_or(0, |(_, n)| *n);
                    // `+ of(&ws)`: one builder per owner, at `PodResources::default()` — the same
                // shape a workspace slot has, which is what `builder_service` gives it.
                let need = u64::from(q.workspaces) * of(&ws) + u64::from(q.environments) * of(&env) + of(&ws);
                let have = u64::from(if dim == Dim::Cpu { q.cpu } else { q.memory_gb });
                assert!(have >= need, "{dim:?}: {have} does not cover {need} for team={team}");
            }
        }
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
