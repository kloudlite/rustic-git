//! Blob pull and the two single-shot push forms. Chunked upload lives in `uploads.rs`.
use super::{auth, oci_err, store::blob_path, Digest};
use crate::http::Trusted;
use crate::App;
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension,
};
use slatedb::object_store::{ObjectStoreExt, PutPayload};
use std::collections::HashMap;
use std::sync::Arc;

/// Largest single layer accepted, checked against the body's size BEFORE it is stored: an
/// unbounded push must not be able to fill a node's disk. Override with RUSTIC_GIT_MAX_LAYER.
pub fn max_layer() -> u64 {
    std::env::var("RUSTIC_GIT_MAX_LAYER").ok().and_then(|v| v.parse().ok())
        .unwrap_or(10 * 1024 * 1024 * 1024)
}

pub async fn get_blob(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, digest)): Path<(String, String, String)>,
) -> Response {
    blob_response(app, trusted, headers, owner, name, digest, true).await
}

pub async fn head_blob(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, digest)): Path<(String, String, String)>,
) -> Response {
    blob_response(app, trusted, headers, owner, name, digest, false).await
}

/// Bumps a blob's object-store mtime via copy-to-self (mirrors `Store::touch_image`'s DB
/// equivalent — there is no dedicated "touch" verb in `object_store`, so a copy onto the same
/// path is the standard way to force a fresh `last_modified`). Only when the existing mtime is
/// already past half the sweep's grace window: a hot pull HEADs the same digest repeatedly, and
/// rewriting the object on every one of those would turn a read into a write for no benefit — a
/// blob younger than half-grace is already safe from the next sweep.
/// ponytail: half-grace is a flat guard, not per-object backoff; revisit if a pathological
/// HEAD-storm on one digest ever shows up as sustained object-store write load.
async fn refresh_blob_mtime(
    app: &App,
    path: &slatedb::object_store::path::Path,
    meta: &slatedb::object_store::ObjectMeta,
) {
    let half_grace = super::gc::blob_grace() / 2;
    let age = chrono::Utc::now().signed_duration_since(meta.last_modified);
    if age < chrono::Duration::from_std(half_grace).unwrap_or(chrono::Duration::zero()) {
        return;
    }
    // Best-effort: a failed touch just means the NEXT HEAD/mount tries again, or the object keeps
    // its old mtime and a sweep landing in the meantime is protected by the double-`referenced()`
    // read instead (see gc.rs) — never worth failing the caller's request over.
    let _ = app.store.os.copy(path, path).await;
}

async fn blob_response(
    app: Arc<App>,
    trusted: Trusted,
    headers: HeaderMap,
    owner: String,
    name: String,
    digest: String,
    with_body: bool,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, false).await {
        return r;
    }
    let Some(d) = Digest::parse(&digest) else {
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "malformed digest");
    };
    let path = blob_path(&owner, &d);
    let meta = match app.store.os.head(&path).await {
        Ok(m) => m,
        Err(slatedb::object_store::Error::NotFound { .. }) => {
            return oci_err(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "no such blob")
        }
        Err(e) => return crate::registry::oci_internal(e.into()),
    };
    let hdrs = [
        (header::CONTENT_LENGTH, meta.size.to_string()),
        (header::CONTENT_TYPE, "application/octet-stream".into()),
        (
            header::HeaderName::from_static("docker-content-digest"),
            d.to_string(),
        ),
    ];
    if !with_body {
        // A HEAD tells the client "this blob exists" without re-uploading it, so it can turn into
        // a reference (a manifest naming this digest) after the sweep's grace window has already
        // judged the blob's upload timestamp too old — see gc.rs's `sweep_owner` doc on the mount
        // race this closes. Errors are swallowed, not surfaced: a failed refresh must not turn a
        // successful HEAD into a 500 — worst case the next HEAD (or the sweep's own grace) covers it.
        refresh_blob_mtime(&app, &path, &meta).await;
        return (StatusCode::OK, hdrs).into_response();
    }
    // Stream the layer straight through: buffering the whole object here is an anonymous
    // memory-DoS for public images (a few concurrent pulls of a large layer OOM the node).
    match app.store.os.get(&path).await {
        Ok(r) => (StatusCode::OK, hdrs, axum::body::Body::from_stream(r.into_stream())).into_response(),
        Err(slatedb::object_store::Error::NotFound { .. }) => {
            oci_err(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "no such blob")
        }
        Err(e) => crate::registry::oci_internal(e.into()),
    }
}

/// `POST /v2/{o}/{n}/blobs/uploads/`
///
/// Three shapes arrive here: `?digest=` with a body (push it now), `?mount=&from=` (cross-repo
/// mount, see below), and bare (open a session, completed via `uploads.rs`'s chunked PATCH or
/// `finish_upload` below).
pub async fn start_upload(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, true).await {
        return r;
    }
    // Cross-repo mount. Blobs are per-OWNER, so a mount inside the team is a no-op — the bytes are
    // already at the path the mounting image reads. Across teams there is nothing to point at, and
    // the spec's fallback is exactly right: 202, and the client uploads it.
    if let (Some(mount), Some(from)) = (q.get("mount"), q.get("from")) {
        let Some(d) = Digest::parse(mount) else {
            return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "malformed digest");
        };
        let from_owner = from.split('/').next().unwrap_or_default();
        let mount_path = blob_path(&owner, &d);
        if from_owner == owner {
            if let Ok(meta) = app.store.os.head(&mount_path).await {
                // Same race as HEAD (see blob_response): the mounting image now references a blob
                // whose own upload timestamp may be long past the sweep's grace window.
                refresh_blob_mtime(&app, &mount_path, &meta).await;
                if let Err(e) = app.store.touch_image(&owner, &name).await {
                    return crate::registry::oci_internal(e);
                }
                return created(&owner, &name, &d);
            }
        }
        return super::uploads::open_session(&app, &owner, &name).await;
    }
    if let Some(digest) = q.get("digest") {
        return finish_blob(&app, &owner, &name, digest, body).await;
    }
    super::uploads::open_session(&app, &owner, &name).await
}

/// `PUT /v2/{o}/{n}/blobs/uploads/{uuid}?digest=` — completes a session. When the body carries the
/// whole blob and no chunk was PATCHed, this is the two-request push.
pub async fn finish_upload(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, uuid)): Path<(String, String, String)>,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, true).await {
        return r;
    }
    let Some(digest) = q.get("digest") else {
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "digest query parameter required");
    };
    super::uploads::complete(&app, &owner, &name, &uuid, digest, &headers, body).await
}

/// Verify and store one whole blob. The digest is checked BEFORE the object lands, so a corrupt
/// layer never becomes readable under a name that promises different bytes.
pub(super) async fn finish_blob(
    app: &App,
    owner: &str,
    name: &str,
    digest: &str,
    body: Bytes,
) -> Response {
    let Some(d) = Digest::parse(digest) else {
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "malformed digest");
    };
    if body.len() as u64 > max_layer() {
        return oci_err(StatusCode::from_u16(413).unwrap(), "SIZE_INVALID", "layer too large");
    }
    // Verified against the algorithm the client CLAIMED (`d.algo`, from the digest it pushed
    // under), not assumed sha256 — a sha512 push must be checked as sha512.
    if Digest::of_algo(&d.algo, &body).as_ref() != Some(&d) {
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "content does not match digest");
    }
    if let Err(e) = app.store.os.put(&blob_path(owner, &d), PutPayload::from(body)).await {
        return crate::registry::oci_internal(e.into());
    }
    // The image now exists, even with no manifest yet: a push that uploads layers and then fails
    // should leave something the owner can see and clean up. `touch_image`, never
    // `set_image_visibility` — a push must not flip a public image back to private.
    if let Err(e) = app.store.touch_image(owner, name).await {
        return crate::registry::oci_internal(e);
    }
    created(owner, name, &d)
}

/// `DELETE /v2/{o}/{n}/blobs/{digest}` — remove the object.
///
/// Deleting here does NOT check whether a manifest still references it: the client asked, the
/// client owns it. What is never done is the reverse — no manifest delete removes a blob. That is
/// the sweeper's job, because only it can see every image that might share the layer.
pub async fn delete_blob(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, digest)): Path<(String, String, String)>,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, true).await {
        return r;
    }
    let Some(d) = Digest::parse(&digest) else {
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "malformed digest");
    };
    match app.store.os.delete(&blob_path(&owner, &d)).await {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(slatedb::object_store::Error::NotFound { .. }) => {
            oci_err(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "no such blob")
        }
        Err(e) => crate::registry::oci_internal(e.into()),
    }
}

pub(super) fn created(owner: &str, name: &str, d: &Digest) -> Response {
    (
        StatusCode::CREATED,
        [
            (header::LOCATION, format!("/v2/{owner}/{name}/blobs/{d}")),
            (
                header::HeaderName::from_static("docker-content-digest"),
                d.to_string(),
            ),
        ],
    )
        .into_response()
}
