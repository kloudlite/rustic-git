//! Resumable blob uploads.
//!
//! A session is two things: a staging object holding the bytes received so far, and a row in the
//! image's database recording how many they are. Both are addressable from any node that owns the
//! image, so a session survives the image moving — nothing about it lives in this process.
use super::{auth, blobs, oci_err, store::blob_path, Digest};
use crate::http::Trusted;
use crate::store::Store;
use crate::App;
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension,
};
use rand::RngCore;
use slatedb::object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use std::sync::Arc;

const SESSION_PREFIX: &str = "upload/";

/// How long an abandoned session may sit before the GC worker sweeps it. Same shape as
/// `blobs::max_layer`: a const default, overridable via env for deployments that want a tighter
/// (or looser) window. Session leak is bounded by grace * max_layer per abandoned push, so this
/// is the other half of the DoS fix `max_layer` alone does not cover.
pub fn upload_grace() -> std::time::Duration {
    std::env::var("RUSTIC_GIT_UPLOAD_GRACE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(std::time::Duration::from_secs)
        .unwrap_or(std::time::Duration::from_secs(24 * 3600))
}

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

/// A 416 that still tells the client where the session stands, per spec: `Range: 0-{have-1}`, or
/// `0-0` when nothing has been received yet — that is what reference registries send for an empty
/// session; there is no "no range" form for a 416 the way there is for a 202/204, since the client
/// asked "where am I" by getting refused, not by asking cleanly. `Docker-Upload-UUID` and
/// `Location` ride along too: a resuming client needs the session's address, not just its offset.
fn range_not_satisfiable(owner: &str, name: &str, uuid: &str, have: u64) -> Response {
    let mut r = oci_err(StatusCode::RANGE_NOT_SATISFIABLE, "BLOB_UPLOAD_INVALID", "chunk does not continue the upload");
    let last = if have == 0 { 0 } else { have - 1 };
    let h = r.headers_mut();
    h.insert(header::RANGE, format!("0-{last}").parse().unwrap());
    h.insert(header::LOCATION, format!("/v2/{owner}/{name}/blobs/uploads/{uuid}").parse().unwrap());
    h.insert(header::HeaderName::from_static("docker-upload-uuid"), uuid.parse().unwrap());
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
    // Two PATCHes to the same session racing would both read the same `have`, both append to the
    // staging object from that offset, and last-writer-wins clobbers the other's bytes (the digest
    // check at PUT time catches it eventually, but as a confusing failure far from the cause).
    // Serialize the whole read-have -> append -> write sequence per session.
    let lock = app.store.keyed_lock(&format!("upload/{owner}/{name}/{uuid}"));
    let _guard = lock.lock().await;
    let have = match received(&app, &owner, &name, &uuid).await {
        Ok(Some(n)) => n,
        Ok(None) => return oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no such upload"),
        Err(e) => return crate::http::internal_pub(e),
    };
    // A Content-Range that does not continue where the session left off is 416. Absent is allowed:
    // a client streaming one chunk need not send it.
    if let Some(cr) = headers.get(header::CONTENT_RANGE).and_then(|v| v.to_str().ok()) {
        let mut parts = cr.trim_start_matches("bytes ").split('-');
        let start: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(u64::MAX);
        let end: Option<u64> = parts.next().and_then(|s| s.parse().ok());
        if start != have {
            return range_not_satisfiable(&owner, &name, &uuid, have);
        }
        // The declared length must match what actually arrived — a header claiming more (or
        // fewer) bytes than the body carries means the client's own bookkeeping is already wrong,
        // and letting the session advance by the real length while it believes otherwise means it
        // desyncs from what's stored.
        if let Some(end) = end {
            // `end + 1` overflows on `bytes 0-18446744073709551615` (u64::MAX): a real chunk can
            // never be that long, so an overflow here is a malformed header, not a valid range —
            // refuse it cleanly instead of panicking in debug / wrapping in release.
            let Some(declared_end) = end.checked_add(1) else {
                return oci_err(
                    StatusCode::BAD_REQUEST,
                    "BLOB_UPLOAD_INVALID",
                    "declared range end is out of bounds",
                );
            };
            if declared_end != have + body.len() as u64 {
                return oci_err(
                    StatusCode::BAD_REQUEST,
                    "BLOB_UPLOAD_INVALID",
                    "declared range length does not match body length",
                );
            }
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
                [
                    (header::HeaderName::from_static("docker-upload-uuid"), uuid.clone()),
                    // A client asks here to RESUME: it needs where to send the next chunk as
                    // much as how far the upload got, so the session's URL travels with it.
                    (header::LOCATION, format!("/v2/{owner}/{name}/blobs/uploads/{uuid}")),
                ],
            )
                .into_response();
            {
                // Always present, `0-0` for an empty session — the resume protocol reads this
                // header unconditionally, and reference registries answer 0-0 rather than
                // omitting it when nothing has landed yet.
                let n = n.max(1);
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
    headers: &axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    if !valid_uuid(uuid) {
        return oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no such upload");
    }
    let Some(d) = Digest::parse(digest) else {
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "malformed digest");
    };
    let have = match received(app, owner, name, uuid).await {
        Ok(Some(n)) => n,
        Ok(None) => return oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no such upload"),
        Err(e) => return crate::http::internal_pub(e),
    };
    // A PUT may carry the final chunk WITH a Content-Range. A start that is not where the
    // session left off is the out-of-order error, not a digest error — the client re-sends the
    // chunk on a 416 but restarts the whole upload on a 400, so conflating them is expensive.
    if let Some(cr) = headers.get(axum::http::header::CONTENT_RANGE).and_then(|v| v.to_str().ok()) {
        let start: u64 = cr
            .trim_start_matches("bytes ")
            .split('-')
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or(u64::MAX);
        if start != have {
            return range_not_satisfiable(owner, name, uuid, have);
        }
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
    // Checked before the hash: no point paying for a whole-buffer sha256 on something we're about
    // to reject for size anyway.
    if buf.len() as u64 > blobs::max_layer() {
        return oci_err(StatusCode::from_u16(413).unwrap(), "SIZE_INVALID", "layer too large");
    }
    // Hashed here, from the staged bytes plus this request's body, because the running hash
    // cannot be carried across requests. See the module note above. Hashed with the CLAIMED
    // algorithm (`d.algo`), not assumed sha256, so a sha512 push is checked as sha512.
    if Digest::of_algo(&d.algo, &buf).as_ref() != Some(&d) {
        // The session stays open: a client that mis-stated the digest may retry the PUT. Only the
        // successful path retires it.
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "content does not match digest");
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

impl Store {
    /// Delete this owner's abandoned upload sessions: the staging object under `uploads/{owner}/`
    /// AND the matching `upload/{uuid}` row in that session's image database. Mirrors
    /// `gc::sweep_owner`'s keep-biased style — an entry this can't read (bad path shape, a listing
    /// hiccup) is skipped, never deleted on uncertainty, and one bad entry does not abort the rest
    /// of the sweep (unlike the blob sweep, a stuck session has no manifest whose correctness
    /// depends on seeing every row, so there is nothing to protect by aborting).
    pub async fn sweep_stale_uploads(&self, owner: &str, grace: std::time::Duration) -> crate::Result<usize> {
        let prefix = slatedb::object_store::path::Path::from(format!("uploads/{owner}"));
        let mut listing = self.os.list(Some(&prefix));
        let cutoff = chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::now() - grace);
        let mut n = 0usize;
        while let Some(m) = futures::StreamExt::next(&mut listing).await {
            let Ok(m) = m else { continue };
            if m.last_modified > cutoff {
                continue;
            }
            // Path is `uploads/{owner}/{name}/{uuid}` — the name segment is needed to find the
            // session's row, since sessions live in the per-IMAGE database, not a per-owner one.
            let parts: Vec<_> = m.location.parts().collect();
            let (Some(uuid), Some(name)) = (parts.last(), parts.get(parts.len().saturating_sub(2)))
            else {
                continue;
            };
            let (uuid, name) = (uuid.as_ref().to_string(), name.as_ref().to_string());
            if self.os.delete(&m.location).await.is_err() {
                continue;
            }
            if let Ok(db) = self.image_db(owner, &name).await {
                let _ = db.delete(session_key(&uuid)).await;
            }
            n += 1;
        }
        Ok(n)
    }
}
