//! The Clusters area: one row per region, one detail page per region, and the three node verbs an
//! operator retiring a VM needs — drain, undrain, decommission.
//!
//! Nothing here decides anything about placement. Drain only sets the label the AGENT already
//! watches (`crd::DECOMMISSION_LABEL`); the drain itself, the `draining …`/`drained …` stamp and
//! every release it implies happen on the node, at its own beat. That is why undrain is a real
//! abort (the agent's beat does nothing at all without the label) and why decommission can be a
//! cordon and nothing more: running work keeps running, and the VM is the operator's to delete.

use super::*;
use k8s_openapi::api::core::v1::Node;

/// A live worktree — `bins/agent`'s `Parent::is_live_worktree`, restated over the CRDs this tier
/// lists directly: an environment's pod set has no single `podRef`, so its phase is all there is.
fn live_workspace(w: &crd::Workspace) -> bool {
    let Some(st) = w.status.as_ref() else { return false };
    st.phase != crd::Phase::Stopped && st.pod_ref.is_some()
}

fn live_environment(e: &crd::Environment) -> bool {
    e.status.as_ref().is_some_and(|st| st.phase != crd::Phase::Stopped)
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClusterRow {
    pub(crate) region: String,
    pub(crate) status: String,
    pub(crate) agents_ready: i64,
    agents_desired: i64,
    nodes_ready: i64,
    nodes_total: i64,
    draining: i64,
    working_copies: i64,
    /// `present` once a `ClusterSettings/default` exists for the region, `absent` while it is
    /// riding env-and-default values.
    // ponytail: `parse-error` is in the spec's vocabulary but unreachable today — a typed decode
    // of the CR fails closed as a 5xx rather than handing back a partial object. Add it here if
    // the read ever becomes untyped.
    pub(crate) settings_status: String,
}

/// The per-region facts every row and the detail both need, read once. `Workspace`/`Environment`
/// carry `spec.region`, so the region filter is a spec read — never the label view.
struct RegionFacts {
    nodes: Vec<Node>,
    workspaces: Vec<crd::Workspace>,
    environments: Vec<crd::Environment>,
}

/// The client that answers for a region — `client_for_region` for an active one, which is the
/// single upgrade point a region -> client map will land on. An INACTIVE region still has to
/// render (and still has to be drainable: deactivate-then-drain is the retirement sequence), and
/// `client_for_region` refuses one on purpose, so it falls back to the same handle here.
async fn region_client<'a>(s: &'a ApiState, r: &crd::Region) -> Result<&'a kube::Client, Response> {
    match r.spec.status == "active" {
        true => super::client_for_region(s, &r.name_any()).await,
        false => kube(s),
    }
}

/// A region that EXISTS, active or not, plus its client. `not_found` for one that does not.
async fn region_of<'a>(s: &'a ApiState, region: &str) -> Result<(crd::Region, &'a kube::Client), Response> {
    let r = Api::<crd::Region>::all(kube(s)?.clone()).get_opt(region).await.map_err(kube_err)?.ok_or_else(not_found)?;
    let client = region_client(s, &r).await?;
    Ok((r, client))
}

async fn facts(client: &kube::Client, region: &str) -> Result<RegionFacts, Response> {
    let nodes = Api::<Node>::all(client.clone()).list(&ListParams::default()).await.map_err(kube_err)?.items;
    let mut workspaces =
        Api::<crd::Workspace>::all(client.clone()).list(&ListParams::default()).await.map_err(kube_err)?.items;
    let mut environments =
        Api::<crd::Environment>::all(client.clone()).list(&ListParams::default()).await.map_err(kube_err)?.items;
    workspaces.retain(|w| w.spec.region == region);
    environments.retain(|e| e.spec.region == region);
    Ok(RegionFacts { nodes, workspaces, environments })
}

fn working_copies(f: &RegionFacts) -> i64 {
    (f.workspaces.iter().filter(|w| live_workspace(w)).count() + f.environments.iter().filter(|e| live_environment(e)).count())
        as i64
}

/// The region's agent DaemonSet, or zeros. A region whose agent has never been deployed is a fact
/// worth showing on the row, not a 5xx that hides every other region with it.
async fn agent_counts(s: &ApiState, region: &str) -> (i64, i64) {
    match super::super::workloads::workload_doc(s, &super::super::workloads::Scope::Region(region.to_string()), "rustic-git-agent")
        .await
    {
        Ok(d) => (d.ready, d.desired),
        Err(_) => (0, 0),
    }
}

/// `absent` while the region rides env-and-default values, `present` once every agent has applied
/// the CR, and `stale (lag N)` in between — `status.observedGeneration` is the generation an agent
/// last applied, so the gap against `metadata.generation` is exactly the number of saves that have
/// not reached the readers yet.
async fn settings_status(client: &kube::Client) -> Result<String, Response> {
    let api: Api<crd::ClusterSettings> = Api::all(client.clone());
    let Some(cs) = api.get_opt("default").await.map_err(kube_err)? else {
        return Ok("absent".into());
    };
    Ok(settings_lag(cs.metadata.generation, cs.status.as_ref().and_then(|st| st.observed_generation)))
}

fn settings_lag(generation: Option<i64>, observed: Option<i64>) -> String {
    match generation.unwrap_or(0) - observed.unwrap_or(0) {
        lag if lag > 0 => format!("stale (lag {lag})"),
        _ => "present".into(),
    }
}

/// One row per region — factored out of the route so Overview can compose from it directly
/// instead of a second walk of `Region`/nodes/workloads.
pub(crate) async fn cluster_rows(s: &ApiState) -> Result<Vec<ClusterRow>, Response> {
    let regions: Vec<crd::Region> =
        Api::<crd::Region>::all(kube(s)?.clone()).list(&ListParams::default()).await.map_err(kube_err)?.items;
    // ponytail: ONE settings read for the whole list, because every region resolves to the same
    // client today (`workloads`' own note). Move it inside the loop, keyed on that region's
    // client, the moment a region -> client map exists.
    let settings_status = settings_status(kube(s)?).await?;
    let mut rows = Vec::with_capacity(regions.len());
    for r in &regions {
        let region = r.name_any();
        let client = region_client(s, r).await?;
        let f = facts(client, &region).await?;
        let (agents_ready, agents_desired) = agent_counts(s, &region).await;
        let docs: Vec<super::NodeDoc> = f.nodes.iter().map(super::node_doc).collect();
        rows.push(ClusterRow {
            region: region.clone(),
            status: r.spec.status.clone(),
            agents_ready,
            agents_desired,
            nodes_ready: docs.iter().filter(|n| n.ready).count() as i64,
            nodes_total: docs.len() as i64,
            draining: docs.iter().filter(|n| n.decommission).count() as i64,
            working_copies: working_copies(&f),
            settings_status: settings_status.clone(),
        });
    }
    Ok(rows)
}

pub(crate) async fn list_clusters(State(s): State<Arc<ApiState>>) -> Result<Response, Response> {
    Ok(Json(cluster_rows(&s).await?).into_response())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NodeRow {
    #[serde(flatten)]
    node: super::NodeDoc,
    /// Live worktrees whose volume sits on this node — the number that says what a drain is
    /// waiting for.
    working_copies: i64,
    replicas_held: i64,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClusterDetail {
    region: String,
    status: String,
    nodes: Vec<NodeRow>,
    workloads: Vec<super::super::workloads::WorkloadDoc>,
    settings: crd::ClusterSettingsSpec,
}

pub(crate) async fn cluster_detail(State(s): State<Arc<ApiState>>, Path(region): Path<String>) -> Result<Response, Response> {
    check_path_segment(&region)?;
    let (r, client) = region_of(&s, &region).await?;
    let f = facts(client, &region).await?;
    let volumes: Vec<crd::Volume> =
        Api::<crd::Volume>::all(client.clone()).list(&ListParams::default()).await.map_err(kube_err)?.items;
    let replicas: Vec<crd::VolumeReplica> =
        Api::<crd::VolumeReplica>::all(client.clone()).list(&ListParams::default()).await.map_err(kube_err)?.items;

    // A parent's volume is what pins it to a node, so "working copies here" is counted the way the
    // agent's own drain counter counts it: the volume's `spec.nodeName`, never the parent's.
    let volume_node = |name: &Option<String>| {
        name.as_ref().and_then(|n| volumes.iter().find(|v| v.name_any() == *n)).map(|v| v.spec.node_name.clone())
    };
    let nodes = f
        .nodes
        .iter()
        .map(|n| {
            let name = n.name_any();
            let here = |vol: Option<String>| volume_node(&vol).as_deref() == Some(name.as_str());
            let ws = f
                .workspaces
                .iter()
                .filter(|w| live_workspace(w) && here(w.status.as_ref().and_then(|st| st.volume_ref.clone())))
                .count();
            let envs = f
                .environments
                .iter()
                .filter(|e| live_environment(e) && here(e.status.as_ref().and_then(|st| st.volume_ref.clone())))
                .count();
            NodeRow {
                node: super::node_doc(n),
                working_copies: (ws + envs) as i64,
                replicas_held: replicas.iter().filter(|r| r.spec.node == name).count() as i64,
            }
        })
        .collect();

    let workloads = super::super::workloads::list_workloads(&s, std::slice::from_ref(&region)).await.unwrap_or_default();
    let settings = Api::<crd::ClusterSettings>::all(client.clone())
        .get_opt("default")
        .await
        .map_err(kube_err)?
        .map(|cs| cs.spec)
        .unwrap_or_default();
    Ok(Json(ClusterDetail { region, status: r.spec.status.clone(), nodes, workloads, settings }).into_response())
}

// ── region status ───────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
pub(crate) struct StatusBody {
    /// `active` or `inactive` — anything else is an activate, same coercion `create_region` makes.
    status: String,
    #[serde(default)]
    note: String,
}

/// Activate / deactivate. Server-side apply of the SAME shape `create_region` writes, because
/// re-registering a region is how its status changes — a second write mechanism would be a second
/// thing that can disagree about what a retired region looks like.
pub(crate) async fn set_region_status(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(region): Path<String>,
    Json(body): Json<StatusBody>,
) -> Result<Response, Response> {
    let c = caller(&s, &headers).await?;
    check_path_segment(&region)?;
    let status = if body.status == "inactive" { "inactive" } else { "active" };
    let note = body.note.trim().to_string();
    // Deactivating stops a whole region being offered; activating only restores what was already
    // registered, so only the loud half demands a reason (Global Constraint).
    if status == "inactive" && note.is_empty() {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, "note is required").into_response());
    }
    let api: Api<crd::Region> = Api::all(kube(&s)?.clone());
    let existing = api.get_opt(&region).await.map_err(kube_err)?.ok_or_else(not_found)?;
    let apply = crd::Region::new(&region, crd::RegionSpec { name: existing.spec.name.clone(), status: status.into() });
    let saved = api
        .patch(&region, &PatchParams::apply("rustic-git-api").force(), &Patch::Apply(&apply))
        .await
        .map_err(kube_err)?;
    let action = if status == "inactive" { "deactivate-region" } else { "activate-region" };
    audit(&s, &c.name, action, &region, (!note.is_empty()).then_some(note), "ok").await;
    Ok(Json(region_doc(&saved)).into_response())
}

// ── drain / undrain / decommission ──────────────────────────────────────

#[derive(serde::Deserialize)]
pub(crate) struct ReasonBody {
    #[serde(default)]
    reason: String,
}

/// The one gate every node verb shares: a reason, a region that exists and is active, and a node
/// that is actually IN that region's cluster. `nodes` cannot be name-restricted in RBAC, so this
/// read is the scoping — the PATCH below always names a node the region just answered for.
async fn target<'a>(
    s: &'a ApiState,
    headers: &axum::http::HeaderMap,
    region: &str,
    node: &str,
    body: &ReasonBody,
) -> Result<(Caller, &'a kube::Client, String, Node), Response> {
    let c = caller(s, headers).await?;
    check_path_segment(region)?;
    check_path_segment(node)?;
    let reason = body.reason.trim().to_string();
    if reason.is_empty() {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, "reason is required").into_response());
    }
    // Inactive is admitted here and nowhere else: a region is deactivated FIRST and its nodes
    // drained after, so refusing one would leave the retirement sequence with no way to finish.
    let (_, client) = region_of(s, region).await?;
    let obj = Api::<Node>::all(client.clone()).get_opt(node).await.map_err(kube_err)?.ok_or_else(not_found)?;
    Ok((c, client, reason, obj))
}

async fn patch_node(client: &kube::Client, node: &str, patch: serde_json::Value) -> Result<Node, Response> {
    Api::<Node>::all(client.clone())
        .patch(node, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .map_err(kube_err)
}

pub(crate) async fn drain(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path((region, node)): Path<(String, String)>,
    Json(body): Json<ReasonBody>,
) -> Result<Response, Response> {
    let (c, client, reason, _) = target(&s, &headers, &region, &node, &body).await?;
    let patch = serde_json::json!({"metadata": {"labels": {crd::DECOMMISSION_LABEL: "true"}}});
    let out = patch_node(client, &node, patch).await?;
    audit(&s, &c.name, "drain", &format!("{region}/{node}"), Some(reason), "ok").await;
    Ok(Json(super::node_doc(&out)).into_response())
}

/// The documented abort. The stamp goes with the label: leaving a stale `drained …` behind would
/// let the next drain's decommission gate open before that drain had done anything.
pub(crate) async fn undrain(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path((region, node)): Path<(String, String)>,
    Json(body): Json<ReasonBody>,
) -> Result<Response, Response> {
    let (c, client, reason, _) = target(&s, &headers, &region, &node, &body).await?;
    let patch = serde_json::json!({"metadata": {
        "labels": {crd::DECOMMISSION_LABEL: serde_json::Value::Null},
        "annotations": {crd::DECOMMISSION_STATUS: serde_json::Value::Null},
    }});
    let out = patch_node(client, &node, patch).await?;
    audit(&s, &c.name, "undrain", &format!("{region}/{node}"), Some(reason), "ok").await;
    Ok(Json(super::node_doc(&out)).into_response())
}

/// Cordon, and nothing else. The console never deletes a VM; what this does is stop new pods
/// landing here and tell the operator the VM may now go. The gate is the agent's own sticky stamp,
/// which is the only thing that knows whether anyone would lose bytes.
pub(crate) async fn decommission(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path((region, node)): Path<(String, String)>,
    Json(body): Json<ReasonBody>,
) -> Result<Response, Response> {
    let (c, client, reason, obj) = target(&s, &headers, &region, &node, &body).await?;
    let drained = obj
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(crd::DECOMMISSION_STATUS))
        .is_some_and(|v| v.starts_with(crd::DRAINED_PREFIX));
    if !drained {
        return Err((StatusCode::CONFLICT, "not drained yet").into_response());
    }
    let out = patch_node(client, &node, serde_json::json!({"spec": {"unschedulable": true}})).await?;
    audit(&s, &c.name, "decommission", &format!("{region}/{node}"), Some(reason), "ok").await;
    Ok(Json(super::node_doc(&out)).into_response())
}

#[cfg(test)]
mod tests {
    /// The lag is what an operator acts on: a save that has not reached the agents yet must not
    /// read as `present`, and a CR nobody has bumped since must not read as stale.
    #[test]
    fn settings_lag_reports_unapplied_generations() {
        assert_eq!(super::settings_lag(Some(3), Some(2)), "stale (lag 1)");
        assert_eq!(super::settings_lag(Some(3), Some(3)), "present");
        assert_eq!(super::settings_lag(Some(1), None), "stale (lag 1)");
        assert_eq!(super::settings_lag(None, None), "present");
    }
}
