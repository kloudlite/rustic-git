//! Blob pull and the two single-shot push forms. Chunked upload lives in `uploads.rs`.
use super::{auth, oci_err, store::blob_path, Digest};
use crate::http::Trusted;
use crate::App;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension,
};
use slatedb::object_store::ObjectStoreExt;
use std::collections::HashMap;
use std::sync::Arc;

/// Largest single layer accepted, checked against the body's size BEFORE it is stored: an
/// unbounded push must not be able to fill a node's disk. Override with RUSTIC_GIT_MAX_LAYER.
///
/// Read once and cached: this is on the hot blob path and the env var never changes after
/// process start.
pub fn max_layer() -> u64 {
    static LAYER: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *LAYER.get_or_init(|| {
        std::env::var("RUSTIC_GIT_MAX_LAYER").ok().and_then(|v| v.parse().ok())
            .unwrap_or(10 * 1024 * 1024 * 1024)
    })
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
    body: Body,
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
        if from_owner == owner && app.store.os.head(&mount_path).await.is_ok() {
            if let Err(e) = app.store.touch_image(&owner, &name).await {
                return crate::registry::oci_internal(e);
            }
            return created(&owner, &name, &d);
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
    body: Body,
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
    body: Body,
) -> Response {
    let Some(d) = Digest::parse(digest) else {
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "malformed digest");
    };
    // Verified against the algorithm the client CLAIMED (`d.algo`, from the digest it pushed
    // under), not assumed sha256. `pour` lands the object only after the hash matches, so a
    // corrupt layer never becomes readable under a name that promises different bytes.
    match super::uploads::pour(&app.store.os, &blob_path(owner, &d), Some(&d), super::uploads::body_stream(body)).await {
        Ok(_) => {}
        Err(super::uploads::Refused::TooLarge) => {
            return oci_err(StatusCode::from_u16(413).unwrap(), "SIZE_INVALID", "layer too large")
        }
        Err(super::uploads::Refused::WrongDigest) => {
            return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "content does not match digest")
        }
        Err(super::uploads::Refused::Failed(e)) => return crate::registry::oci_internal(e),
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
