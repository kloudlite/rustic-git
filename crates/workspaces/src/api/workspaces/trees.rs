//! `POST/DELETE /v1/workspaces/{id}/trees[/{name}]` — a subagent's writable working directory.
//!
//! A tree is a nested btrfs subvolume inside the workspace's own subvolume, cut by the node agent
//! on this ask. `/v1` writes `spec.trees` and nothing else: it establishes no fact about whether
//! the subvolume exists, which is why both verbs answer 202 and the caller waits on
//! `status.trees[name].ready`.
//!
//! Quota is deliberately NOT charged. A tree is bytes on a volume the owner already pays for and
//! CPU in a pod already sized; what bounds it is `trees_per_workspace`, refused here at the ask.

use super::*;
use crate::crd::{tree_name_ok, tree_path, TreeSpec};


#[derive(serde::Deserialize)]
pub(crate) struct NewTree {
    name: String,
}


#[derive(serde::Serialize)]
struct TreeDoc {
    name: String,
    path: String,
}


/// How many times a write re-reads and re-decides after losing the CAS below — the same number and
/// the same reasoning as `INTERCEPT_ATTEMPTS`: three lost races in a row is contention a retry
/// will not fix.
const TREE_ATTEMPTS: usize = 3;


/// Compare-and-set `spec.trees`, guarded by the `resourceVersion` the decision was read against.
///
/// A merge patch REPLACES a JSON array wholesale, so two agents asking for a tree in the same
/// instant would each write the list they read and one ask would vanish with a 202 over it. `add`
/// rather than `replace` because `spec.trees` is `#[serde(default)]`: an object written before the
/// field existed has no such key, and `replace` on a missing path is a 422 this would retry
/// forever.
///
/// `Ok(None)` is a lost race; an `Err` is an outage.
// Two static JSON pointers and the serialisation of our own list: none can fail, and an attribute
// on an expression inside the vec is not stable Rust, so the allow sits here.
#[allow(clippy::expect_used)]
async fn cas_trees(c: &kube::Client, w: &crd::Workspace, want: Vec<TreeSpec>) -> Result<bool, Response> {
    let api: Api<crd::Workspace> = Api::all(c.clone());
    let ops = json_patch::Patch(vec![
        json_patch::PatchOperation::Test(json_patch::TestOperation {
            path: "/metadata/resourceVersion".parse().expect("a static pointer"),
            value: serde_json::json!(w.resource_version().unwrap_or_default()),
        }),
        json_patch::PatchOperation::Add(json_patch::AddOperation {
            path: "/spec/trees".parse().expect("a static pointer"),
            value: serde_json::to_value(want).expect("trees serialize"),
        }),
    ]);
    match api.patch(&w.name_any(), &PatchParams::default(), &Patch::Json::<crd::Workspace>(ops)).await {
        Ok(_) => Ok(true),
        Err(kube::Error::Api(st)) if st.code == 409 || st.code == 422 => Ok(false),
        Err(err) => Err(kube_err(err)),
    }
}


fn contended() -> Response {
    (StatusCode::CONFLICT, "the workspace was changed while this was being written; try again").into_response()
}


/// The region's ceiling: `ClusterSettings/default` if an admin set one, else this tier's own
/// resolved value. Read per request rather than cached — the same rule quota usage follows, for
/// the same reason: a stale ceiling can only be wrong in the direction that hands out more.
async fn ceiling(s: &ApiState) -> u32 {
    let stored = match kube(s) {
        Ok(k) => Api::<crd::ClusterSettings>::all(k.clone())
            .get_opt("default")
            .await
            .ok()
            .flatten()
            .and_then(|c| c.spec.trees_per_workspace),
        Err(_) => None,
    };
    stored.unwrap_or_else(|| s.settings.load().trees_per_workspace)
}


/// Ask for a tree. Spec only — the node agent cuts the subvolume — so this is a 202 over a name
/// that does not exist on disk yet.
pub(crate) async fn cut_tree(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    method: axum::http::Method,
    uri: axum::extract::OriginalUri,
    Path(id): Path<String>,
    Json(body): Json<NewTree>,
) -> Result<Response, Response> {
    let caller_id = caller_for(&s, &headers, &method, uri.path()).await?;
    if !tree_name_ok(&body.name) {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "a tree name is 1 to 32 characters of a-z, 0-9 and -".to_string(),
        )
            .into_response());
    }
    let limit = ceiling(&s).await;
    for _ in 0..TREE_ATTEMPTS {
        let w = my_ws(&s, &caller_id, &id).await?;
        // A tree is a snapshot of a LIVE working directory: on a stopped workspace no node holds
        // the subvolume and there is nothing to cut, so this is a refusal rather than a wish left
        // pending until somebody happens to start it.
        if w.spec.desired_state != DesiredState::Running {
            return Err((
                StatusCode::CONFLICT,
                "the workspace is not running; a tree is cut from a live one".to_string(),
            )
                .into_response());
        }
        if w.spec.trees.iter().any(|t| t.name == body.name) {
            return Err((StatusCode::CONFLICT, format!("tree {} exists", body.name)).into_response());
        }
        let held = w.spec.trees.len() as u32;
        if held >= limit {
            return Err((StatusCode::CONFLICT, format!("trees: {held} of {limit} in use")).into_response());
        }
        let mut want = w.spec.trees.clone();
        want.push(TreeSpec { name: body.name.clone(), created: chrono::Utc::now().to_rfc3339() });
        if cas_trees(kube(&s)?, &w, want).await? {
            // `w.spec.name`, never `id`: the pod mounts at the display name, so the CR id names a path
            // that does not exist (R-D21).
            let doc = TreeDoc { name: body.name.clone(), path: tree_path(&body.name) };
            return Ok((StatusCode::ACCEPTED, Json(doc)).into_response());
        }
    }
    Err(contended())
}


/// Give a tree back. The spec entry goes now; the subvolume goes on the agent's next pass, and
/// `status.trees` drops the row once it has.
///
/// 404 rather than an idempotent 202 on an unknown name: the caller waits on a name it believes it
/// created, and a silent success would hide a typo for as long as it kept waiting.
pub(crate) async fn drop_tree(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    method: axum::http::Method,
    uri: axum::extract::OriginalUri,
    Path((id, name)): Path<(String, String)>,
) -> Result<Response, Response> {
    let caller_id = caller_for(&s, &headers, &method, uri.path()).await?;
    for _ in 0..TREE_ATTEMPTS {
        let w = my_ws(&s, &caller_id, &id).await?;
        if !w.spec.trees.iter().any(|t| t.name == name) {
            return Err(not_found());
        }
        let want: Vec<TreeSpec> = w.spec.trees.iter().filter(|t| t.name != name).cloned().collect();
        if cas_trees(kube(&s)?, &w, want).await? {
            return Ok(StatusCode::ACCEPTED.into_response());
        }
    }
    Err(contended())
}
