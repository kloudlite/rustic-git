//! Resumable blob uploads.
//!
//! A session is two things: a staging object holding the bytes received so far, and a row in the
//! image's database recording how many they are. Both are addressable from any node that owns the
//! image, so a session survives the image moving — nothing about it lives in this process.
use super::{auth, blobs, oci_err, store::blob_path, Digest};
use crate::http::Trusted;
use crate::App;
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension,
};
use rand::RngCore;
use slatedb::object_store::{ObjectStoreExt, PutPayload};
use std::sync::Arc;

const SESSION_PREFIX: &str = "upload/";

fn staging(owner: &str, name: &str, uuid: &str) -> slatedb::object_store::path::Path {
    slatedb::object_store::path::Path::from(format!("uploads/{owner}/{name}/{uuid}"))
}

fn session_key(uuid: &str) -> Vec<u8> {
    format!("{SESSION_PREFIX}{uuid}").into_bytes()
}

fn new_uuid() -> String {
    let mut buf = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// A uuid, and nothing that could be a path. Generated here, checked on the way back in: a session
/// id from a client is a path segment, and a path segment is never trusted.
fn valid_uuid(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

async fn received(app: &App, owner: &str, name: &str, uuid: &str) -> crate::Result<Option<u64>> {
    let db = app.store.image_db(owner, name).await?;
    Ok(db
        .get(session_key(uuid))
        .await?
        .and_then(|v| String::from_utf8_lossy(&v).parse().ok()))
}

/// `POST /v2/{o}/{n}/blobs/uploads/` with no `digest` — opens a session the client completes with
/// a PUT or PATCHes chunks into.
pub async fn open_session(app: &App, owner: &str, name: &str) -> Response {
    let uuid = new_uuid();
    // The image must exist (even manifest-less) before its database can hold a session row.
    if let Err(e) = app.store.touch_image(owner, name).await {
        return crate::http::internal_pub(e);
    }
    let db = match app.store.image_db(owner, name).await {
        Ok(db) => db,
        Err(e) => return crate::http::internal_pub(e),
    };
    if let Err(e) = db.put(session_key(&uuid), b"0".as_slice()).await {
        return crate::http::internal_pub(e.into());
    }
    accepted(owner, name, &uuid, 0)
}

/// 202 with the session's URL and how much of the blob it holds. `Range` is inclusive and a
/// session holding nothing has no range at all — a `0-0` there would claim one byte.
fn accepted(owner: &str, name: &str, uuid: &str, len: u64) -> Response {
    let mut r = (
        StatusCode::ACCEPTED,
        [
            (header::LOCATION, format!("/v2/{owner}/{name}/blobs/uploads/{uuid}")),
            (header::HeaderName::from_static("docker-upload-uuid"), uuid.to_string()),
        ],
    )
        .into_response();
    if len > 0 {
        r.headers_mut().insert(header::RANGE, format!("0-{}", len - 1).parse().unwrap());
    }
    r
}

/// `PATCH` — one chunk. Ranges must be contiguous, per the spec: a gap is 416, and so is a chunk
/// that would rewrite bytes already received.
pub async fn patch(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, uuid)): Path<(String, String, String)>,
    body: Bytes,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, true).await {
        return r;
    }
    if !valid_uuid(&uuid) {
        return oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no such upload");
    }
    let have = match received(&app, &owner, &name, &uuid).await {
        Ok(Some(n)) => n,
        Ok(None) => return oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no such upload"),
        Err(e) => return crate::http::internal_pub(e),
    };
    // A Content-Range that does not continue where the session left off is 416. Absent is allowed:
    // a client streaming one chunk need not send it.
    if let Some(cr) = headers.get(header::CONTENT_RANGE).and_then(|v| v.to_str().ok()) {
        let start: u64 = cr
            .trim_start_matches("bytes ")
            .split('-')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(u64::MAX);
        if start != have {
            return oci_err(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "BLOB_UPLOAD_INVALID",
                "chunk does not continue the upload",
            );
        }
    }
    // Note: axum's DefaultBodyLimit on the blob routes is set to max_layer() (Task 5), which caps
    // a WHOLE LAYER, not a chunk — a chunk is smaller by definition, so it never trips that limit
    // on its own. What DOES need checking here is the running total: a session could otherwise be
    // grown past max_layer() one small chunk at a time.
    if have + body.len() as u64 > blobs::max_layer() {
        return oci_err(StatusCode::from_u16(413).unwrap(), "SIZE_INVALID", "layer too large");
    }
    // ponytail: read-modify-write of the staging object, so a chunked push of an N-byte layer
    // moves O(N * chunks) bytes. Correct and stateless, which is what makes a session survive the
    // image moving nodes. Swap for the object store's multipart API if large pushes get slow —
    // that needs the part list persisted alongside the byte count.
    let path = staging(&owner, &name, &uuid);
    let mut buf = match app.store.os.get(&path).await {
        Ok(r) => match r.bytes().await {
            Ok(b) => b.to_vec(),
            Err(e) => return crate::http::internal_pub(e.into()),
        },
        Err(slatedb::object_store::Error::NotFound { .. }) => vec![],
        Err(e) => return crate::http::internal_pub(e.into()),
    };
    buf.extend_from_slice(&body);
    let len = buf.len() as u64;
    if let Err(e) = app.store.os.put(&path, PutPayload::from(buf)).await {
        return crate::http::internal_pub(e.into());
    }
    let db = match app.store.image_db(&owner, &name).await {
        Ok(d) => d,
        Err(e) => return crate::http::internal_pub(e),
    };
    if let Err(e) = db.put(session_key(&uuid), len.to_string().into_bytes()).await {
        return crate::http::internal_pub(e.into());
    }
    accepted(&owner, &name, &uuid, len)
}

/// `GET` — how far the session got. 204 with a `Range`, per the spec. A WRITE check, not a read
/// one: an upload session is not published data, and its progress is not a public read.
pub async fn status(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, uuid)): Path<(String, String, String)>,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, true).await {
        return r;
    }
    if !valid_uuid(&uuid) {
        return oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no such upload");
    }
    match received(&app, &owner, &name, &uuid).await {
        Ok(Some(n)) => {
            let mut r = (
                StatusCode::NO_CONTENT,
                [(header::HeaderName::from_static("docker-upload-uuid"), uuid.clone())],
            )
                .into_response();
            if n > 0 {
                r.headers_mut().insert(header::RANGE, format!("0-{}", n - 1).parse().unwrap());
            }
            r
        }
        Ok(None) => oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no such upload"),
        Err(e) => crate::http::internal_pub(e),
    }
}

/// `DELETE` — cancel. Idempotent in effect: the staged bytes and the row both go.
pub async fn cancel(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, uuid)): Path<(String, String, String)>,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, true).await {
        return r;
    }
    if !valid_uuid(&uuid) {
        return oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no such upload");
    }
    discard(&app, &owner, &name, &uuid).await;
    StatusCode::NO_CONTENT.into_response()
}

async fn discard(app: &App, owner: &str, name: &str, uuid: &str) {
    let _ = app.store.os.delete(&staging(owner, name, uuid)).await;
    if let Ok(db) = app.store.image_db(owner, name).await {
        let _ = db.delete(session_key(uuid)).await;
    }
}

/// `PUT /v2/{o}/{n}/blobs/uploads/{uuid}?digest=` — completes a session. A body here is the last
/// chunk, which is how the two-request push (no PATCH ever sent) arrives.
///
// ponytail: completion re-reads the staged object to hash it — one extra read per layer. The
// alternative is a resumable hasher (sha2 has no serializable state) or holding the hasher in
// node memory, which loses the session when the image moves nodes. Revisit if layer pushes show
// up in a profile.
pub async fn complete(
    app: &App,
    owner: &str,
    name: &str,
    uuid: &str,
    digest: &str,
    body: Bytes,
) -> Response {
    if !valid_uuid(uuid) {
        return oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no such upload");
    }
    let Some(d) = Digest::parse(digest) else {
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "malformed digest");
    };
    match received(app, owner, name, uuid).await {
        Ok(Some(_)) => {}
        Ok(None) => return oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no such upload"),
        Err(e) => return crate::http::internal_pub(e),
    }
    let path = staging(owner, name, uuid);
    let mut buf = match app.store.os.get(&path).await {
        Ok(r) => match r.bytes().await {
            Ok(b) => b.to_vec(),
            Err(e) => return crate::http::internal_pub(e.into()),
        },
        Err(slatedb::object_store::Error::NotFound { .. }) => vec![],
        Err(e) => return crate::http::internal_pub(e.into()),
    };
    buf.extend_from_slice(&body);
    // Hashed here, from the staged bytes plus this request's body, because the running hash
    // cannot be carried across requests. See the module note above.
    if Digest::of(&buf) != d {
        // The session stays open: a client that mis-stated the digest may retry the PUT. Only the
        // successful path retires it.
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "content does not match digest");
    }
    if buf.len() as u64 > blobs::max_layer() {
        return oci_err(StatusCode::from_u16(413).unwrap(), "SIZE_INVALID", "layer too large");
    }
    if let Err(e) = app.store.os.put(&blob_path(owner, &d), PutPayload::from(buf)).await {
        return crate::http::internal_pub(e.into());
    }
    if let Err(e) = app.store.touch_image(owner, name).await {
        return crate::http::internal_pub(e);
    }
    discard(app, owner, name, uuid).await;
    blobs::created(owner, name, &d)
}
