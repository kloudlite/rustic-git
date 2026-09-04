//! `GET /admin/owners` and `GET /admin/owners/{slug}` — the Owners area's list and detail, both
//! composed from reads that already exist elsewhere (`quota::{usage,effective}`, the workspace
//! and environment listers, the volumes query, the request queue, the audit log). Nothing new is
//! computed here; this module only assembles what an operator needs to see about one owner
//! without a second click, per the console's "one page, one question" rule.

use super::*;

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OwnerRow {
    owner: String,
    is_team: bool,
    limit: crd::QuotaSpec,
    used: crate::quota::Usage,
    /// `"own"` when the owner has an explicit `Quota` object, `"default"` when they are riding the
    /// `default-user`/`default-team` fallback — the web needs this to know whether "Edit" starts
    /// from a real object or from the compiled-in table.
    source: &'static str,
    /// A `QuotaRequest` still `Pending` for this owner — the list's badge, so an operator does not
    /// have to open every row to find who is waiting.
    pending: bool,
}

/// Every owner who has a `Quota`, has ever opened a `QuotaRequest`, or has a live `Workspace`,
/// `Environment` or `Volume` — wider than the old `usage_all`, which only saw the first two.
/// `spec.owner` on each kind, never the label: the label is a view, this is the enumeration the
/// Global Constraint requires the truth to come from.
async fn all_owners(client: &kube::Client) -> Result<std::collections::BTreeSet<String>, Response> {
    let quotas: Api<crd::Quota> = Api::all(client.clone());
    let reqs: Api<crd::QuotaRequest> = Api::all(client.clone());
    let ws: Api<crd::Workspace> = Api::all(client.clone());
    let envs: Api<crd::Environment> = Api::all(client.clone());
    let vols: Api<crd::Volume> = Api::all(client.clone());

    let mut owners: std::collections::BTreeSet<String> = quotas
        .list(&ListParams::default())
        .await
        .map_err(kube_err)?
        .items
        .into_iter()
        .map(|q| q.name_any())
        .filter(|n| n != crd::DEFAULT_USER_QUOTA && n != crd::DEFAULT_TEAM_QUOTA)
        .collect();
    owners.extend(reqs.list(&ListParams::default()).await.map_err(kube_err)?.items.into_iter().map(|r| r.spec.owner));
    owners.extend(ws.list(&ListParams::default()).await.map_err(kube_err)?.items.into_iter().map(|w| w.spec.owner));
    owners.extend(envs.list(&ListParams::default()).await.map_err(kube_err)?.items.into_iter().map(|e| e.spec.owner));
    owners.extend(vols.list(&ListParams::default()).await.map_err(kube_err)?.items.into_iter().map(|v| v.spec.owner));
    Ok(owners)
}

pub(crate) async fn owners_list(State(s): State<Arc<ApiState>>) -> Result<Response, Response> {
    let client = kube(&s)?;
    let owners = all_owners(client).await?;
    let quotas: Api<crd::Quota> = Api::all(client.clone());
    let reqs: Api<crd::QuotaRequest> = Api::all(client.clone());
    let pending_owners: std::collections::HashSet<String> = reqs
        .list(&ListParams::default())
        .await
        .map_err(kube_err)?
        .items
        .into_iter()
        .filter(is_pending)
        .map(|r| r.spec.owner)
        .collect();

    let mut rows = Vec::with_capacity(owners.len());
    for owner in owners {
        let is_team = scope::is_team(&s, &owner).await;
        let own = quotas.get_opt(&owner).await.map_err(kube_err)?;
        let source = if own.is_some() { "own" } else { "default" };
        let limit = match &own {
            Some(q) => q.spec.clone(),
            None => crate::quota::effective(client, &owner, is_team).await.map_err(kube_err)?,
        };
        let used = crate::quota::usage(client, &owner).await.map_err(kube_err)?;
        let pending = pending_owners.contains(&owner);
        rows.push(OwnerRow { owner, is_team, limit, used, source, pending });
    }
    // Tightest-first: the operator's question is "who is closest to their limit", so the row with
    // the least headroom on ANY dimension sorts to the top. `f64` because the six dimensions have
    // different units; a ratio is the only thing that compares them.
    rows.sort_by(|a, b| tightest_ratio(b).total_cmp(&tightest_ratio(a)));
    Ok(Json(rows).into_response())
}

/// The smallest (limit - used)/limit across the six dimensions, i.e. the dimension closest to
/// being exhausted. `f64::MAX` for a zero limit (nothing to divide by, and a zero-limit dimension
/// reads as maximally tight anyway by every other row's standard).
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
    if l <= 0.0 {
        return f64::MAX;
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
    let is_team = scope::is_team(&s, &owner).await;
    let quotas: Api<crd::Quota> = Api::all(client.clone());
    let own = quotas.get_opt(&owner).await.map_err(kube_err)?;
    let source = if own.is_some() { "own" } else { "default" };
    let limit = match &own {
        Some(q) => q.spec.clone(),
        None => crate::quota::effective(&client, &owner, is_team).await.map_err(kube_err)?,
    };
    let used = crate::quota::usage(&client, &owner).await.map_err(kube_err)?;

    // The claim already authorized acting on `owner` (`refuse_without_claim`), so these reuse the
    // owner-scoped readers directly rather than going back through `may_act_on`.
    let workspaces = super::super::workspaces::ws_for_owner(&s, &owner).await?;
    let environments = super::super::environments::envs_for(&s, std::slice::from_ref(&owner)).await?;
    let volumes = super::super::volumes::volumes_for(&s, std::slice::from_ref(&owner), None).await?;

    let reqs_api: Api<crd::QuotaRequest> = Api::all(client.clone());
    let mut requests = reqs_api.list(&ListParams::default()).await.map_err(kube_err)?.items;
    requests.retain(|r| r.spec.owner == owner);
    requests.sort_by(|a, b| b.metadata.creation_timestamp.cmp(&a.metadata.creation_timestamp));
    requests.truncate(5);
    let requests: Vec<_> = requests.iter().map(super::super::request_doc).collect();

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
