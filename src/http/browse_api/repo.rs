//! Repo-scoped read routes: refs, tree/blob browsing, log, commit detail, and signatures.
use super::{odb_json, open_ro, parse_oid, BLOB_CAP};
use super::super::{internal, Trusted};
use crate::App;
use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Serialize)]
pub(super) struct Ref {
    name: String,
    oid: String,
    kind: &'static str,
}

pub(super) async fn api_refs(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    let repo = match open_ro(&app, &trusted, &headers, &owner, &name).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let refs = match app.store.list_refs(&repo).await {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let out: Vec<Ref> = refs
        .into_iter()
        .map(|(name, oid)| Ref {
            kind: if name.starts_with("refs/tags/") { "tag" } else { "branch" },
            name,
            oid: oid.to_hex().to_string(),
        })
        .collect();
    Json(out).into_response()
}

pub(super) async fn tree(
    app: Arc<App>,
    trusted: Trusted,
    headers: HeaderMap,
    owner: String,
    name: String,
    oid: String,
    path: String,
) -> Response {
    let repo = match open_ro(&app, &trusted, &headers, &owner, &name).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let oid = match parse_oid(&oid) {
        Ok(o) => o,
        Err(r) => return r,
    };
    odb_json(repo, move |odb| crate::browse::tree_at(odb, oid, &path)).await
}

pub(super) async fn api_tree_root(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, oid)): Path<(String, String, String)>,
) -> Response {
    tree(app, trusted, headers, owner, name, oid, String::new()).await
}

pub(super) async fn api_tree(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, oid, path)): Path<(String, String, String, String)>,
) -> Response {
    tree(app, trusted, headers, owner, name, oid, path).await
}

pub(super) async fn api_blob(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, oid, path)): Path<(String, String, String, String)>,
) -> Response {
    let repo = match open_ro(&app, &trusted, &headers, &owner, &name).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let oid = match parse_oid(&oid) {
        Ok(o) => o,
        Err(r) => return r,
    };
    odb_json(repo, move |odb| crate::browse::blob_at(odb, oid, &path, BLOB_CAP)).await
}

pub(super) async fn api_log(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, oid)): Path<(String, String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let repo = match open_ro(&app, &trusted, &headers, &owner, &name).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let oid = match parse_oid(&oid) {
        Ok(o) => o,
        Err(r) => return r,
    };
    // Clamped, not rejected: `n` is a page size, and a client asking for a million commits wants
    // the first page, not an error.
    let n = q
        .get("n")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(50)
        .clamp(1, 200);
    odb_json(repo, move |odb| crate::browse::log(odb, oid, n)).await
}

#[derive(Serialize)]
pub(super) struct CommitDetail {
    #[serde(flatten)]
    commit: crate::browse::Commit,
    diff: String,
}

pub(super) async fn api_commit(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, oid)): Path<(String, String, String)>,
) -> Response {
    let repo = match open_ro(&app, &trusted, &headers, &owner, &name).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let oid = match parse_oid(&oid) {
        Ok(o) => o,
        Err(r) => return r,
    };
    odb_json(repo, move |odb| {
        crate::browse::commit(odb, oid).map(|(commit, diff)| CommitDetail { commit, diff })
    })
    .await
}

/// Every file under a commit, in one answer. See `browse::files_at` — the caller
/// wants the shape of the repo, and a request per directory is what this replaces.
pub(super) async fn api_files(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, oid)): Path<(String, String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let repo = match open_ro(&app, &trusted, &headers, &owner, &name).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let oid = match parse_oid(&oid) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let path = q.get("path").cloned().unwrap_or_default();
    // Clamped rather than refused, exactly as `log` clamps `n`.
    let cap = q.get("cap").and_then(|v| v.parse::<usize>().ok()).unwrap_or(5000).clamp(1, 20_000);
    odb_json(repo, move |odb| crate::browse::files_at(odb, oid, &path, cap)).await
}

#[derive(Serialize)]
pub(super) struct LastChange {
    name: String,
    #[serde(flatten)]
    commit: crate::browse::Commit,
}

/// What last touched each entry of a directory. One walk of history for the whole
/// directory; see `browse::last_changes`.
pub(super) async fn api_lastmod(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, oid)): Path<(String, String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let repo = match open_ro(&app, &trusted, &headers, &owner, &name).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let oid = match parse_oid(&oid) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let path = q.get("path").cloned().unwrap_or_default();
    let budget = q.get("budget").and_then(|v| v.parse::<usize>().ok()).unwrap_or(500).clamp(1, 2000);
    odb_json(repo, move |odb| {
        crate::browse::last_changes(odb, oid, &path, budget)
            .map(|v| v.into_iter().map(|(name, commit)| LastChange { name, commit }).collect::<Vec<_>>())
    })
    .await
}

#[derive(Serialize)]
pub(super) struct SignatureOf {
    signature: String,
    /// Base64: the payload is raw object bytes, which JSON cannot carry.
    payload_base64: String,
    author_email: String,
}

/// A commit's signature and the bytes it covers.
///
/// The node can produce these but cannot judge them — it has no list of whose
/// keys are whose. Verification belongs to the api tier, which does.
pub(super) async fn api_signature(
    State(app): State<Arc<App>>,
    axum::Extension(trusted): axum::Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, oid)): Path<(String, String, String)>,
) -> Response {
    let repo = match open_ro(&app, &trusted, &headers, &owner, &name).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let oid = match parse_oid(&oid) {
        Ok(o) => o,
        Err(r) => return r,
    };
    odb_json(repo, move |odb| {
        crate::browse::signature_of(odb, oid).map(|s| {
            s.map(|s| {
                use base64::Engine;
                SignatureOf {
                    signature: s.signature,
                    payload_base64: base64::engine::general_purpose::STANDARD.encode(&s.payload),
                    author_email: s.author_email,
                }
            })
        })
    })
    .await
}
