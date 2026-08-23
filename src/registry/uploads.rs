//! Resumable blob uploads.
//!
//! A session is its staging object and nothing else: `uploads/{owner}/{name}/{uuid}` holds the
//! bytes received so far, and its size IS how many there are. Addressable from any node that owns
//! the image, so a session survives the image moving — and, because there is no row in the
//! image's database, the GC worker can sweep an abandoned one without opening a database it does
//! not own (which would fence the node that does).
use super::{auth, blobs, oci_err, store::blob_path, store::Hasher, Digest};
use crate::http::Trusted;
use crate::store::Store;
use crate::App;
use axum::{
    body::{Body, Bytes},
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension,
};
use futures::{stream::BoxStream, Stream, StreamExt, TryStreamExt};
use rand::RngCore;
use slatedb::object_store::{path::Path as OsPath, ObjectStore, ObjectStoreExt, PutPayload, WriteMultipart};
use std::sync::Arc;

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

fn staging(owner: &str, name: &str, uuid: &str) -> OsPath {
    OsPath::from(format!("uploads/{owner}/{name}/{uuid}"))
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

/// Why a stream could not be stored. Split so the handler can pick the status: the size cap and
/// a digest mismatch are the client's fault and keep the session; anything else is a 500.
pub(super) enum Refused {
    TooLarge,
    WrongDigest,
    Failed(crate::Error),
}

/// How many parts may be in flight before `pour` waits: bounds memory at `(1 + this) * 5 MiB`
/// per request while still overlapping network with hashing.
const IN_FLIGHT: usize = 4;

/// Streams `src` to `dest` through a multipart upload, hashing as it goes when `expect` names a
/// digest to verify against. Memory is one 5 MiB part plus `IN_FLIGHT` more, never the layer.
/// The object lands only on `finish`; every refusal aborts first, so nothing half-written — or
/// wrongly named — is ever readable under `dest`. Returns the byte count written.
pub(super) async fn pour<S>(
    os: &Arc<dyn ObjectStore>,
    dest: &OsPath,
    expect: Option<&Digest>,
    mut src: S,
) -> Result<u64, Refused>
where
    S: Stream<Item = crate::Result<Bytes>> + Unpin,
{
    let upload = os.put_multipart(dest).await.map_err(|e| Refused::Failed(e.into()))?;
    let mut w = WriteMultipart::new(upload);
    let mut hasher = expect.and_then(|d| Hasher::new(&d.algo));
    let mut n = 0u64;
    while let Some(chunk) = src.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                let _ = w.abort().await;
                return Err(Refused::Failed(e));
            }
        };
        n += chunk.len() as u64;
        if n > blobs::max_layer() {
            let _ = w.abort().await;
            return Err(Refused::TooLarge);
        }
        if let Some(h) = hasher.as_mut() {
            h.update(&chunk);
        }
        if let Err(e) = w.wait_for_capacity(IN_FLIGHT).await {
            let _ = w.abort().await;
            return Err(Refused::Failed(e.into()));
        }
        w.put(chunk);
    }
    if let Some(want) = expect {
        if hasher.map(Hasher::finish).as_ref() != Some(want) {
            let _ = w.abort().await;
            return Err(Refused::WrongDigest);
        }
    }
    w.finish().await.map_err(|e| Refused::Failed(e.into()))?;
    Ok(n)
}

/// The session's bytes so far, as a stream — empty when there is no staging object, which is how a
/// two-request push (no PATCH ever sent) arrives at `complete`.
pub(super) async fn staged(
    os: &Arc<dyn ObjectStore>,
    path: &OsPath,
) -> crate::Result<BoxStream<'static, crate::Result<Bytes>>> {
    match os.get(path).await {
        Ok(r) => Ok(r.into_stream().map_err(crate::Error::from).boxed()),
        Err(slatedb::object_store::Error::NotFound { .. }) => Ok(futures::stream::empty().boxed()),
        Err(e) => Err(e.into()),
    }
}

pub(super) fn body_stream(body: Body) -> BoxStream<'static, crate::Result<Bytes>> {
    body.into_data_stream().map_err(|e| crate::err(e.to_string())).boxed()
}

pub(super) fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok())
}

/// The spec's `Content-Range` on a chunk. A start that is not where the session left off is 416
/// (with the headers a client resumes from — see `range_not_satisfiable`); absent is allowed, a
/// client streaming one chunk need not send it. Returns the length the header DECLARES, if it
/// declares one, so the caller can hold the body to it: a header claiming more (or fewer) bytes
/// than arrive means the client's own bookkeeping is wrong, and advancing the session by the real
/// length while it believes otherwise desyncs it from what is stored.
pub(super) fn declared_chunk(
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    uuid: &str,
    have: u64,
) -> Result<Option<u64>, Response> {
    let Some(cr) = headers.get(header::CONTENT_RANGE).and_then(|v| v.to_str().ok()) else {
        return Ok(None);
    };
    let mut parts = cr.trim_start_matches("bytes ").split('-');
    let start: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(u64::MAX);
    let end: Option<u64> = parts.next().and_then(|s| s.parse().ok());
    if start != have {
        return Err(range_not_satisfiable(owner, name, uuid, have));
    }
    let Some(end) = end else { return Ok(None) };
    // `end + 1` overflows on `bytes 0-18446744073709551615`: a real chunk can never be that long,
    // so an overflow here is a malformed header, not a valid range — refuse it cleanly instead of
    // panicking in debug / wrapping in release. Same for an end before the start.
    match end.checked_add(1).and_then(|e| e.checked_sub(have)) {
        Some(len) => Ok(Some(len)),
        None => Err(oci_err(
            StatusCode::BAD_REQUEST,
            "BLOB_UPLOAD_INVALID",
            "declared range end is out of bounds",
        )),
    }
}

pub(super) fn length_mismatch() -> Response {
    oci_err(
        StatusCode::BAD_REQUEST,
        "BLOB_UPLOAD_INVALID",
        "declared range length does not match body length",
    )
}

/// How many bytes the session holds, or `None` when there is no session. The staging object is
/// the session: a `NotFound` here is a session that was never opened, was completed, was
/// cancelled, or was swept — all the same answer to a client.
async fn received(app: &App, owner: &str, name: &str, uuid: &str) -> crate::Result<Option<u64>> {
    match app.store.os.head(&staging(owner, name, uuid)).await {
        Ok(m) => Ok(Some(m.size)),
        Err(slatedb::object_store::Error::NotFound { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// `POST /v2/{o}/{n}/blobs/uploads/` with no `digest` — opens a session the client completes with
/// a PUT or PATCHes chunks into.
pub async fn open_session(app: &App, owner: &str, name: &str) -> Response {
    let uuid = new_uuid();
    // The image must exist (even manifest-less) so a completed upload has somewhere to belong.
    if let Err(e) = app.store.touch_image(owner, name).await {
        return crate::registry::oci_internal(e);
    }
    // An EMPTY staging object, written now: the object is the session, so a session with no
    // bytes yet must still be something `received` can find and the sweep can age out.
    if let Err(e) = app.store.os.put(&staging(owner, name, &uuid), PutPayload::default()).await {
        return crate::registry::oci_internal(e.into());
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
    body: Body,
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
        Err(e) => return crate::registry::oci_internal(e),
    };
    let declared = match declared_chunk(&headers, &owner, &name, &uuid, have) {
        Ok(d) => d,
        Err(r) => return r,
    };
    // Checked against Content-Length BEFORE any byte moves, when the client declared one (every
    // real client does). A chunked body is checked after it lands — see below.
    if let (Some(d), Some(cl)) = (declared, content_length(&headers)) {
        if d != cl {
            return length_mismatch();
        }
    }
    // ponytail: the staging object is re-streamed behind each chunk, so a chunked push of an
    // N-byte layer moves O(N * chunks) bytes through the store — but never through memory.
    // Stateless, which is what lets a session survive the image moving nodes. Persist the
    // multipart id + part list in the staging object's sidecar if large chunked pushes get slow.
    let path = staging(&owner, &name, &uuid);
    let src = match staged(&app.store.os, &path).await {
        Ok(s) => s,
        Err(e) => return crate::registry::oci_internal(e),
    };
    let len = match pour(&app.store.os, &path, None, src.chain(body_stream(body))).await {
        Ok(len) => len,
        Err(Refused::TooLarge) => {
            return oci_err(StatusCode::from_u16(413).unwrap(), "SIZE_INVALID", "layer too large")
        }
        Err(Refused::WrongDigest) => unreachable!("no digest expected on a chunk"),
        Err(Refused::Failed(e)) => return crate::registry::oci_internal(e),
    };
    // A chunked body with a Content-Range that lied: the session has advanced by what really
    // arrived, and the 400 tells the client so. Its next GET/PATCH sees the true `Range` — that
    // is the resume protocol working, not a corrupted session.
    if declared.is_some_and(|d| d != len - have) {
        return length_mismatch();
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
        Err(e) => crate::registry::oci_internal(e),
    }
}

/// `DELETE` — cancel. Idempotent in effect: the staging object goes, and it was the whole session.
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
}

/// `PUT /v2/{o}/{n}/blobs/uploads/{uuid}?digest=` — completes a session. A body here is the last
/// chunk, which is how the two-request push (no PATCH ever sent) arrives.
///
// ponytail: completion re-streams the staged object to hash it — one extra read per layer. The
// alternative is a resumable hasher (sha2 has no serializable state) or holding the hasher in
// node memory, which loses the session when the image moves nodes. Revisit if layer pushes show
// up in a profile.
pub async fn complete(
    app: &App,
    owner: &str,
    name: &str,
    uuid: &str,
    digest: &str,
    headers: &HeaderMap,
    body: Body,
) -> Response {
    if !valid_uuid(uuid) {
        return oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no such upload");
    }
    let Some(d) = Digest::parse(digest) else {
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "malformed digest");
    };
    // Same session lock `patch` takes (identical key), held across the same read-have -> read
    // staging -> write sequence: a PATCH racing this PUT would otherwise interleave with the
    // append below, surfacing as a DIGEST_INVALID far from the real cause.
    let lock = app.store.keyed_lock(&format!("upload/{owner}/{name}/{uuid}"));
    let _guard = lock.lock().await;
    let have = match received(app, owner, name, uuid).await {
        Ok(Some(n)) => n,
        Ok(None) => return oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no such upload"),
        Err(e) => return crate::registry::oci_internal(e),
    };
    // A PUT may carry the final chunk WITH a Content-Range. A start that is not where the
    // session left off is the out-of-order error, not a digest error — the client re-sends the
    // chunk on a 416 but restarts the whole upload on a 400, so conflating them is expensive.
    let declared = match declared_chunk(headers, owner, name, uuid, have) {
        Ok(d) => d,
        Err(r) => return r,
    };
    if let (Some(d), Some(cl)) = (declared, content_length(headers)) {
        if d != cl {
            return length_mismatch();
        }
    }
    let src = match staged(&app.store.os, &staging(owner, name, uuid)).await {
        Ok(s) => s,
        Err(e) => return crate::registry::oci_internal(e),
    };
    // Hashed with the CLAIMED algorithm (`d.algo`), not assumed sha256, so a sha512 push is
    // checked as sha512. A mismatch aborts the upload before anything lands under the digest,
    // and the session stays open: a client that mis-stated the digest may retry the PUT.
    let len =
        match pour(&app.store.os, &blob_path(owner, &d), Some(&d), src.chain(body_stream(body))).await {
            Ok(len) => len,
            Err(Refused::TooLarge) => {
                return oci_err(StatusCode::from_u16(413).unwrap(), "SIZE_INVALID", "layer too large")
            }
            Err(Refused::WrongDigest) => {
                return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "content does not match digest")
            }
            Err(Refused::Failed(e)) => return crate::registry::oci_internal(e),
        };
    // The blob has landed under a digest that matched — content-addressed, so a lying
    // Content-Range on a chunked body costs the client a 400 and a retry, never a wrong object.
    if declared.is_some_and(|d| d != len - have) {
        return length_mismatch();
    }
    if let Err(e) = app.store.touch_image(owner, name).await {
        return crate::registry::oci_internal(e);
    }
    discard(app, owner, name, uuid).await;
    blobs::created(owner, name, &d)
}

impl Store {
    /// Delete this owner's abandoned upload sessions — the staging objects under
    /// `uploads/{owner}/` older than `grace`. Object-store reads and deletes ONLY: this runs in
    /// the GC worker, which must never open an image database (the single-opener invariant), and
    /// since the object is the whole session there is nothing else to remove. Keep-biased like
    /// `gc::sweep_owner`: an entry this can't read is skipped, never deleted on uncertainty, and
    /// one bad entry does not abort the rest.
    ///
    // ponytail: `upload/{uuid}` rows written by the pre-row-less build are orphaned — a few bytes
    // each in an image's DB, and nothing deletes them. Upgrade path: a one-off `delete_image_rows`
    // -style prefix purge over the owner's images, if the bytes ever matter.
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
            if self.os.delete(&m.location).await.is_ok() {
                n += 1;
            }
        }
        Ok(n)
    }
}
