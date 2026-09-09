//! Editing a workspace's package list: locks resolved before anything is written, and the one
//! route that re-resolves existing entries.

use super::*;


#[derive(serde::Deserialize)]
pub(crate) struct PackagesBody {
    packages: Vec<String>,
}


/// Change the declared package list. A merge patch on `spec.packages` alone, for the same reason
/// `set_desired` is one: this handler was sent one field and must not claim ownership of a spec
/// the caller never wrote.
pub(crate) async fn patch_ws_packages(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PackagesBody>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers).await?;
    let w = my_ws(&s, &owner, &id).await?;
    crate::packages::validate_list(&body.packages).map_err(bad_packages)?;
    let locks = lock_for(&s, &body.packages, &w.spec.locks, false).await?;
    let api: Api<crd::Workspace> = Api::all(kube(&s)?.clone());
    // `locks` moves with `packages` in ONE patch: a spec carrying a `@` entry with no lock, even
    // for an instant, is a spec the agent would try to build.
    let patch = serde_json::json!({"spec": {"packages": body.packages, "locks": locks}});
    let w = api
        .patch(&id, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .map_err(kube_err)?;
    let pushed = pushed_volumes(&s, kube(&s)?, &owner).await?;
    Ok(Json(ws_doc(&w, &pushed)).into_response())
}


/// Re-resolve every pinned entry against the index, bypassing the cache. The declared list is
/// untouched — only what the pins point at moves — so this takes no body.
///
/// An index outage keeps the locks it had (`lock_all`'s rule) and answers 200: "I could not
/// check" is not a reason to take a working version away from a workspace.
pub(crate) async fn update_ws_packages(
    State(s): State<Arc<ApiState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Response> {
    let owner = caller(&s, &headers).await?;
    let w = my_ws(&s, &owner, &id).await?;
    let locks = lock_for(&s, &w.spec.packages, &w.spec.locks, true).await?;
    let api: Api<crd::Workspace> = Api::all(kube(&s)?.clone());
    let patch = serde_json::json!({"spec": {"locks": locks}});
    let w = api
        .patch(&id, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .map_err(kube_err)?;
    let pushed = pushed_volumes(&s, kube(&s)?, &owner).await?;
    Ok(Json(ws_doc(&w, &pushed)).into_response())
}
