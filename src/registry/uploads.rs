//! Opening and completing an upload session. Chunked PATCH, status, and cancel are Task 6 — this
//! only covers the two-request whole-blob push, which is all this task's tests need.
use super::oci_err;
use crate::App;
use axum::body::Bytes;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use rand::RngCore;

const SESSION_PREFIX: &str = "upload/";

fn session_key(uuid: &str) -> Vec<u8> {
    format!("{SESSION_PREFIX}{uuid}").into_bytes()
}

fn new_uuid() -> String {
    let mut buf = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// `POST /v2/{o}/{n}/blobs/uploads/` with no `digest` — opens a session the client completes with
/// a PUT (this task) or PATCHes into (Task 6).
pub async fn open_session(app: &App, owner: &str, name: &str) -> Response {
    let uuid = new_uuid();
    let db = match app.store.image_db(owner, name).await {
        Ok(db) => db,
        Err(e) => return crate::http::internal_pub(e),
    };
    if let Err(e) = db.put(session_key(&uuid), b"1".as_slice()).await {
        return crate::http::internal_pub(e.into());
    }
    (
        StatusCode::ACCEPTED,
        [
            (
                header::LOCATION,
                format!("/v2/{owner}/{name}/blobs/uploads/{uuid}"),
            ),
            (
                header::HeaderName::from_static("docker-upload-uuid"),
                uuid.clone(),
            ),
        ],
    )
        .into_response()
}

/// `PUT /v2/{o}/{n}/blobs/uploads/{uuid}?digest=` — the session must exist, and the whole body
/// (this task never PATCHed a chunk in) must match `digest` before it is stored.
pub async fn complete(
    app: &App,
    owner: &str,
    name: &str,
    uuid: &str,
    digest: &str,
    body: Bytes,
) -> Response {
    let db = match app.store.image_db(owner, name).await {
        Ok(db) => db,
        Err(e) => return crate::http::internal_pub(e),
    };
    match db.get(session_key(uuid)).await {
        Ok(Some(_)) => {}
        Ok(None) => return oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no such upload"),
        Err(e) => return crate::http::internal_pub(e.into()),
    }
    let resp = super::blobs::finish_blob(app, owner, name, digest, body).await;
    if resp.status() == StatusCode::CREATED {
        if let Err(e) = db.delete(session_key(uuid)).await {
            return crate::http::internal_pub(e.into());
        }
    }
    resp
}
