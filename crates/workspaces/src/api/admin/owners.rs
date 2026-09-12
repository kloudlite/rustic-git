//! `GET /admin/owners` and `GET /admin/owners/{slug}` — the Owners area's list and detail, both
//! composed from reads that already exist elsewhere (`quota::usage`'s per-item logic, folded in
//! memory here instead of per-owner; the workspace and environment listers, the volumes query,
//! the request queue, the audit log). Nothing new is computed here; this module only assembles
//! what an operator needs to see about one owner without a second click, per the console's
//! "one page, one question" rule.

use super::*;
use std::collections::{BTreeSet, HashMap, HashSet};

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OwnerRow {
    pub(crate) owner: String,
    pub(crate) is_team: bool,
    pub(crate) limit: crd::QuotaSpec,
    pub(crate) used: crate::quota::Usage,
    /// `"own"` when the owner has an explicit `Quota` object, `"default"` when they are riding the
    /// `default-user`/`default-team` fallback — the web needs this to know whether "Edit" starts
    /// from a real object or from the compiled-in table.
    pub(crate) source: &'static str,
    /// A `QuotaRequest` still `Pending` for this owner — the list's badge, so an operator does not
    /// have to open every row to find who is waiting.
    pub(crate) pending: bool,
}

/// `owner` is really a team once it names one of the two reserved default objects, no matter what
/// the directory says — the same override `quota::effective` makes, kept in step here so a row
/// for `default-team` (which can appear if it ever gets its own `QuotaRequest`) does not read as
/// a person.
pub(crate) fn team_of(owner: &str, directory_says: bool) -> bool {
    match owner {
        crd::DEFAULT_TEAM_QUOTA => true,
        crd::DEFAULT_USER_QUOTA => false,
        _ => directory_says,
    }
}

/// The owner's own `Quota` if it has one, else the matching default object, else the compiled-in
/// table — `quota::effective`'s exact fallback chain, but taking the owner's own `Quota` (already
/// read once by the caller, from a listing or a single `get_opt`) instead of reading it again:
/// `quota::effective` re-runs `get_opt(owner)` internally, which would be a second read of the
/// same object this module already has in hand.
pub(crate) fn fallback_quota(quota_by_name: &HashMap<String, crd::QuotaSpec>, team: bool) -> crd::QuotaSpec {
    let fallback = if team { crd::DEFAULT_TEAM_QUOTA } else { crd::DEFAULT_USER_QUOTA };
    quota_by_name.get(fallback).cloned().unwrap_or_else(|| crd::default_quota(team))
}

/// Everything six single list calls can answer: every owner, every `Quota`, every pending
/// request, and the raw `Workspace`/`Environment`/`Volume`/`Snapshot` rows usage is folded from.
/// One call per kind regardless of how many owners exist — the N+1 the per-owner version made
/// (`quota::usage` and `Quota::get_opt` called once per owner) is what this replaces.
#[derive(Default)]
pub(crate) struct Fleet {
    pub(crate) quota_by_name: HashMap<String, crd::QuotaSpec>,
    pub(crate) pending_owners: HashSet<String>,
    pub(crate) usage_by_owner: HashMap<String, crate::quota::Usage>,
    pub(crate) owners: BTreeSet<String>,
    // Raw rows, kept alongside the folded `usage_by_owner` so a caller that needs a dimension
    // `Fleet` does not fold (Overview's per-region split — `crate::quota::Usage` has no region)
    // reads the same six list calls instead of re-listing them.
    pub(crate) ws: Vec<Arc<crd::Workspace>>,
    pub(crate) envs: Vec<Arc<crd::Environment>>,
    pub(crate) vols: Vec<Arc<crd::Volume>>,
    pub(crate) snaps: Vec<Arc<crd::Snapshot>>,
}

///  is the admin process's stores when it has them; a caller without an  (the
/// history beat) passes  and lists, exactly as before the cache existed.
pub(crate) async fn fleet(cache: Option<&fleet::FleetCache>, client: &kube::Client) -> Result<Fleet, Response> {
    let quotas = fleet::all(cache, client, |c| &c.quotas).await?;
    let reqs = fleet::all(cache, client, |c| &c.quota_requests).await?;
    let ws = fleet::all(cache, client, |c| &c.workspaces).await?;
    let envs = fleet::all(cache, client, |c| &c.environments).await?;
    let vols = fleet::all(cache, client, |c| &c.volumes).await?;
    let snaps = fleet::all(cache, client, |c| &c.snapshots).await?;

    let mut owners: BTreeSet<String> = quotas
        .iter()
        .map(|q| q.name_any())
        .filter(|n| n != crd::DEFAULT_USER_QUOTA && n != crd::DEFAULT_TEAM_QUOTA)
        .collect();
    owners.extend(reqs.iter().map(|r| r.spec.owner.clone()));
    owners.extend(ws.iter().map(|w| w.spec.owner.clone()));
    owners.extend(envs.iter().map(|e| e.spec.owner.clone()));
    owners.extend(vols.iter().map(|v| v.spec.owner.clone()));

    let pending_owners = reqs.iter().filter(|r| is_pending(r)).map(|r| r.spec.owner.clone()).collect();
    let quota_by_name = quotas.iter().map(|q| (q.name_any(), q.spec.clone())).collect();
    let usage_by_owner = fold_usage(&ws, &envs, &vols, &snaps);

    Ok(Fleet { quota_by_name, pending_owners, usage_by_owner, owners, ws, envs, vols, snaps })
}

/// `quota::usage`'s per-item accounting, done once over every owner's objects instead of once per
/// owner — the SAME rules (a team workspace or volume is charged to `spec.team`, the hidden
/// builder never spends an `environments` slot, a service's own resources beat the env unit, a
/// snapshot follows its volume and counts only when `is_snapshot`), grouped in memory rather than
/// re-listed per owner. A number here that differs from the gate's is a grant made off the wrong
/// figure, which is what this page existed to avoid.
fn fold_usage(
    ws: &[Arc<crd::Workspace>],
    envs: &[Arc<crd::Environment>],
    vols: &[Arc<crd::Volume>],
    snaps: &[Arc<crd::Snapshot>],
) -> HashMap<String, crate::quota::Usage> {
    let mut millis: HashMap<String, u64> = HashMap::new();
    let mut mib: HashMap<String, u64> = HashMap::new();
    let mut out: HashMap<String, crate::quota::Usage> = HashMap::new();

    let charged = |owner: &str, team: &str| if team.is_empty() { owner.to_string() } else { team.to_string() };
    for w in ws {
        let key = charged(&w.spec.owner, &w.spec.team);
        let u = out.entry(key.clone()).or_default();
        u.workspaces += 1;
        if w.spec.desired_state == crd::DesiredState::Running {
            *millis.entry(key.clone()).or_default() += crate::quota::millicores(&w.spec.resources.cpu_limit);
            *mib.entry(key).or_default() += crate::quota::mebibytes(&w.spec.resources.memory_limit);
        }
    }
    for e in envs {
        let u = out.entry(e.spec.owner.clone()).or_default();
        if e.spec.system.is_none() {
            u.environments += 1;
        }
        if e.spec.desired_state == crd::DesiredState::Running {
            for svc in &e.spec.services {
                let unit = svc.resources.clone().unwrap_or_else(crate::k8s::env_unit_resources);
                *millis.entry(e.spec.owner.clone()).or_default() += crate::quota::millicores(&unit.cpu_limit);
                *mib.entry(e.spec.owner.clone()).or_default() += crate::quota::mebibytes(&unit.memory_limit);
            }
        }
    }
    let mut volume_charge: HashMap<String, String> = HashMap::new();
    for v in vols {
        let key = charged(&v.spec.owner, &v.spec.team);
        out.entry(key.clone()).or_default().disk_gb += v.spec.quota_gb;
        volume_charge.insert(kube::ResourceExt::name_any(&**v), key);
    }
    for s in snaps {
        if s.is_snapshot() {
            let key = volume_charge.get(&s.spec.volume).cloned().unwrap_or_else(|| s.spec.owner.clone());
            out.entry(key).or_default().snapshots += 1;
        }
    }
    for (owner, u) in out.iter_mut() {
        u.cpu = millis.get(owner).copied().unwrap_or(0).div_ceil(1000) as u32;
        u.memory_gb = mib.get(owner).copied().unwrap_or(0).div_ceil(1024) as u32;
    }
    out
}

/// The list's rows, tightest-first — factored out of the route so Overview can compose fleet
/// totals from the same six list calls instead of re-listing.
pub(crate) async fn owner_rows(s: &ApiState) -> Result<Vec<OwnerRow>, Response> {
    let client = kube(s)?;
    let f = fleet(s.fleet.as_deref(), client).await?;

    // One `is_team` per row, awaited in turn, made this page's latency the owner count times a
    // directory round trip (2026-09-12). The lookups are independent, so they go out together.
    let owners: Vec<String> = f.owners.into_iter().collect();
    let says: Vec<bool> =
        futures::future::join_all(owners.iter().map(|o| scope::is_team(s, o))).await;

    let mut rows = Vec::with_capacity(owners.len());
    for (owner, directory_says) in owners.into_iter().zip(says) {
        let is_team = team_of(&owner, directory_says);
        let own = f.quota_by_name.get(&owner).cloned();
        let source = if own.is_some() { "own" } else { "default" };
        let limit = own.unwrap_or_else(|| fallback_quota(&f.quota_by_name, is_team));
        let used = f.usage_by_owner.get(&owner).cloned().unwrap_or_default();
        let pending = f.pending_owners.contains(&owner);
        rows.push(OwnerRow { owner, is_team, limit, used, source, pending });
    }
    // Tightest first: the operator's question is "who is closest to their limit", so the row with
    // the least headroom on ANY dimension sorts to the top — ascending by that row's own smallest
    // ratio puts the tightest row (smallest ratio) at index 0.
    rows.sort_by(|a, b| tightest_ratio(a).total_cmp(&tightest_ratio(b)));
    Ok(rows)
}

pub(crate) async fn owners_list(State(s): State<Arc<ApiState>>) -> Result<Response, Response> {
    Ok(Json(owner_rows(&s).await?).into_response())
}

/// The smallest (limit - used)/limit across the six dimensions, i.e. the dimension closest to
/// being exhausted — the row's own tightest number, and what the list sorts ascending by.
fn tightest_ratio(row: &OwnerRow) -> f64 {
    use crate::quota::Dim;
    [Dim::Workspaces, Dim::Environments, Dim::Snapshots, Dim::DiskGb, Dim::Cpu, Dim::MemoryGb]
        .into_iter()
        .map(|d| ratio_of(d, &row.limit, &row.used))
        .fold(f64::MAX, f64::min)
}

fn ratio_of(dim: crate::quota::Dim, limit: &crd::QuotaSpec, used: &crate::quota::Usage) -> f64 {
    use crate::quota::Dim;
    let (u, l) = match dim {
        Dim::Workspaces => (used.workspaces as f64, limit.workspaces as f64),
        Dim::Environments => (used.environments as f64, limit.environments as f64),
        Dim::Snapshots => (used.snapshots as f64, limit.snapshots as f64),
        Dim::DiskGb => (used.disk_gb as f64, limit.disk_gb as f64),
        Dim::Cpu => (used.cpu as f64, limit.cpu as f64),
        Dim::MemoryGb => (used.memory_gb as f64, limit.memory_gb as f64),
    };
    // A zero limit has nothing to divide by, and reads as maximally tight (never "infinite
    // headroom") — negative infinity so it always sorts first, ahead of a row that is merely at
    // 100% of a positive limit.
    if l <= 0.0 {
        return f64::NEG_INFINITY;
    }
    (l - u) / l
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OwnerDetail {
    owner: String,
    is_team: bool,
    limit: crd::QuotaSpec,
    used: crate::quota::Usage,
    source: &'static str,
    workspaces: Vec<crate::model::Workspace>,
    environments: Vec<crate::model::Environment>,
    volumes: Vec<super::super::volumes::VolumeSummary>,
    requests: Vec<super::super::QuotaRequestDoc>,
    audit: Vec<crate::audit::AuditEntry>,
}

pub(crate) async fn owner_detail(
    State(s): State<Arc<ApiState>>,
    _headers: axum::http::HeaderMap,
    Path(owner): Path<String>,
) -> Result<Response, Response> {
    check_path_segment(&owner)?;
    let client = kube(&s)?.clone();
    let directory_says = scope::is_team(&s, &owner).await;
    let is_team = team_of(&owner, directory_says);
    let quotas: Api<crd::Quota> = Api::all(client.clone());
    let own = quotas.get_opt(&owner).await.map_err(kube_err)?;
    let source = if own.is_some() { "own" } else { "default" };
    let limit = match own {
        Some(q) => q.spec,
        // One extra read, for the DEFAULT object — never a second read of `owner`'s own `Quota`,
        // which `quota::effective` would make by calling `get_opt(owner)` again internally.
        None => {
            let fallback = if is_team { crd::DEFAULT_TEAM_QUOTA } else { crd::DEFAULT_USER_QUOTA };
            match quotas.get_opt(fallback).await.map_err(kube_err)? {
                Some(q) => q.spec,
                None => crd::default_quota(is_team),
            }
        }
    };
    let used = crate::quota::usage(&client, &owner).await.map_err(kube_err)?;

    // The claim already authorized acting on `owner` (`refuse_without_claim`), so these reuse the
    // owner-scoped readers directly rather than going back through `may_act_on`. `ws_for_owner` is
    // the read-only half of `list_for_owner` — the key-minting side effect stays on the `/v1`
    // wrapper, never here, since this is a GET the operator did not ask to write anything from.
    let workspaces = super::super::workspaces::ws_for_owner(&s, &owner).await?;
    let environments = super::super::environments::envs_for(&s, std::slice::from_ref(&owner)).await?;
    let volumes = super::super::volumes::volumes_for(&s, std::slice::from_ref(&owner), None).await?;

    let mut requests = fleet::all(s.fleet.as_deref(), &client, |c| &c.quota_requests).await?;
    requests.retain(|r| r.spec.owner == owner);
    requests.sort_by(|a, b| b.metadata.creation_timestamp.cmp(&a.metadata.creation_timestamp));
    requests.truncate(5);
    let requests: Vec<_> = requests.iter().map(|r| super::super::request_doc(r)).collect();

    let audit = match s.keys.as_ref() {
        Some(store) => {
            let filter = crate::audit::AuditFilter { target: Some(owner.clone()), ..Default::default() };
            crate::audit::list(&store.os, filter, None, 10)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())?
                .rows
        }
        // No object store configured (dev only) is the same "nowhere to read from" case
        // `admin::audit`'s handler already 503s on — an empty audit section here is the honest
        // answer for a detail page that must still render its other four sections.
        None => Vec::new(),
    };

    Ok(Json(OwnerDetail { owner, is_team, limit, used, source, workspaces, environments, volumes, requests, audit }).into_response())
}
