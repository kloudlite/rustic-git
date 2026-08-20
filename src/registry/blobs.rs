//! Blob pull and the two single-shot push forms. Chunked upload is `uploads.rs` (next task).
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
        Err(e) => return crate::http::internal_pub(e.into()),
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
    // ponytail: whole-blob read. Layers are capped by max_layer, and the object store client
    // buffers anyway; stream with `get`'s ByteStream if large-layer memory ever shows up in a
    // profile.
    match app.store.os.get(&path).await {
        Ok(r) => match r.bytes().await {
            Ok(b) => (StatusCode::OK, hdrs, b).into_response(),
            Err(e) => crate::http::internal_pub(e.into()),
        },
        Err(e) => crate::http::internal_pub(e.into()),
    }
}

/// `POST /v2/{o}/{n}/blobs/uploads/`
///
/// Three shapes arrive here: `?digest=` with a body (push it now), `?mount=&from=` (Task 7), and
/// bare (open a session, completed by `finish_upload` below). Only the first and third are
/// implemented in this task.
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
    super::uploads::complete(&app, &owner, &name, &uuid, digest, body).await
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
    if Digest::of(&body) != d {
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "content does not match digest");
    }
    if let Err(e) = app.store.os.put(&blob_path(owner, &d), PutPayload::from(body)).await {
        return crate::http::internal_pub(e.into());
    }
    // The image now exists, even with no manifest yet: a push that uploads layers and then fails
    // should leave something the owner can see and clean up. `touch_image`, never
    // `set_image_visibility` — a push must not flip a public image back to private.
    if let Err(e) = app.store.touch_image(owner, name).await {
        return crate::http::internal_pub(e);
    }
    created(owner, name, &d)
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
