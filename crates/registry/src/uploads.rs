//! Resumable blob uploads.
//!
//! A session is `uploads/{owner}/{name}/{uuid}` — the staging object — and, when the backend
//! offers a resumable multipart API, a sidecar at `{uuid}.parts` beside it. Both are plain
//! object-store keys, so a session survives the image moving nodes, and the GC worker can sweep an
//! abandoned one without opening a database it does not own (which would fence the node that does).
//! `valid_uuid` forbids `.`, so a sidecar key can never be mistaken for a session's.
//!
//! Two ways a chunk lands, and which applies is decided per PATCH:
//!
//! * **Fast path** (`Store::mp` is `Some`): every chunk is uploaded once, as `UploadPart`s of a
//!   multipart upload whose id and part ids live in the sidecar. Chunks below S3's 5 MiB part
//!   floor accumulate in the sidecar's tail until they fill a part, so a client chunking at 1 MiB
//!   rewrites at most a 5 MiB tail per PATCH instead of re-streaming the session. Completion is
//!   `CompleteMultipartUpload` — no byte is re-sent.
//! * **Fallback** (no `MultipartStore` — `LocalFileSystem`, i.e. `file://` dev mode): the chunk
//!   is appended by re-streaming the staging object through a fresh multipart. O(N·K), dev only.
//!
//! The sidecar carries the trailing bytes of a chunk that were too few to be a part ("the tail")
//! along with the part list, in ONE object: split across two objects there is no write order that
//! is not torn by a crash — either the tail is counted twice or the parts are lost.
use super::{auth, blobs, oci_err, store::blob_path, store::Hasher, store::ImageExt, Digest};
use crate::Trusted;
use crate::dbstore::Store;
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

fn sidecar_path(owner: &str, name: &str, uuid: &str) -> OsPath {
    OsPath::from(format!("uploads/{owner}/{name}/{uuid}.parts"))
}

/// S3 (and R2, and GCS) refuse any part but the last below 5 MiB. A chunk that cannot reach this
/// on its own — with whatever tail the session already holds — has to go down the append fallback.
const MIN_PART: u64 = 5 * 1024 * 1024;

/// The resumable half of a chunked session: the backend's upload id, the parts it has accepted in
/// order, and the bytes received after the last of them. One object, written whole, because the
/// tail and the part list must move together (see the module comment).
#[derive(serde::Serialize, serde::Deserialize)]
struct Meta {
    id: String,
    /// `PartId::content_id`, in part-index order.
    parts: Vec<String>,
    /// Bytes held by `parts`. The tail is not counted here.
    len: u64,
}

struct Sidecar {
    meta: Meta,
    tail: Bytes,
}

impl Sidecar {
    /// Bytes the session has accepted. Parts plus tail: a client resumes from here.
    fn received(&self) -> u64 {
        self.meta.len + self.tail.len() as u64
    }

    /// JSON header, newline, raw tail. Not JSON all through because the tail is up to 5 MiB of
    /// arbitrary bytes and base64ing it on every chunk is pure waste; neither an upload id nor a
    /// part id can contain a raw newline, and serde escapes them regardless.
    fn encode(&self) -> crate::Result<PutPayload> {
        let mut v = serde_json::to_vec(&self.meta)?;
        v.push(b'\n');
        v.extend_from_slice(&self.tail);
        Ok(PutPayload::from(v))
    }

    fn decode(raw: Bytes) -> crate::Result<Sidecar> {
        let cut = raw
            .iter()
            .position(|b| *b == b'\n')
            .ok_or_else(|| crate::err("upload sidecar has no header"))?;
        Ok(Sidecar { meta: serde_json::from_slice(&raw[..cut])?, tail: raw.slice(cut + 1..) })
    }
}

/// The session, or `None` when there is none. The STAGING object is what says a session exists —
/// `open_session` writes it empty and `discard` deletes it first — so an orphan sidecar left by a
/// crash between the two deletes answers 404 like anything else, and the sweep reaps it.
async fn session(
    app: &App,
    owner: &str,
    name: &str,
    uuid: &str,
) -> crate::Result<Option<(u64, Option<Sidecar>)>> {
    let staged_len = match app.store.os.head(&staging(owner, name, uuid)).await {
        Ok(m) => m.size,
        Err(slatedb::object_store::Error::NotFound { .. }) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    match app.store.os.get(&sidecar_path(owner, name, uuid)).await {
        Ok(r) => {
            let sc = Sidecar::decode(r.bytes().await?)?;
            Ok(Some((sc.received(), Some(sc))))
        }
        Err(slatedb::object_store::Error::NotFound { .. }) => Ok(Some((staged_len, None))),
        Err(e) => Err(e.into()),
    }
}

/// Stream `src` into parts of an existing multipart upload, `MIN_PART` at a time. Returns the part
/// ids accepted, the bytes they hold, and the remainder — which the caller keeps as the session's
/// tail so the NEXT chunk carries it into a full-size part. With `last` there is no next chunk, so
/// the remainder becomes the final part, which is the one S3 exempts from the 5 MiB floor.
///
/// Memory is one part plus one body chunk, never the layer: the tail handed in is under `MIN_PART`
/// by construction, and the buffer is flushed the moment it reaches it.
async fn put_parts<S>(
    mp: &Arc<dyn slatedb::object_store::multipart::MultipartStore>,
    path: &OsPath,
    id: &str,
    mut next_idx: usize,
    mut src: S,
    last: bool,
    mut room: u64,
) -> Result<(Vec<String>, u64, Bytes), Refused>
where
    S: Stream<Item = crate::Result<Bytes>> + Unpin,
{
    let id = id.to_string();
    let mut ids = Vec::new();
    let mut parted = 0u64;
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = src.next().await {
        let chunk = chunk.map_err(Refused::Failed)?;
        room = room.checked_sub(chunk.len() as u64).ok_or(Refused::TooLarge)?;
        buf.extend_from_slice(&chunk);
        if buf.len() as u64 >= MIN_PART {
            parted += buf.len() as u64;
            let payload = PutPayload::from(std::mem::take(&mut buf));
            let p = mp
                .put_part(path, &id, next_idx, payload)
                .await
                .map_err(|e| Refused::Failed(e.into()))?;
            next_idx += 1;
            ids.push(p.content_id);
        }
    }
    if last && !buf.is_empty() {
        parted += buf.len() as u64;
        let payload = PutPayload::from(std::mem::take(&mut buf));
        let p = mp
            .put_part(path, &id, next_idx, payload)
            .await
            .map_err(|e| Refused::Failed(e.into()))?;
        ids.push(p.content_id);
    }
    Ok((ids, parted, Bytes::from(buf)))
}

/// Feeds bytes into a `Hasher` off the tokio worker thread: sha256/sha512 is CPU-bound, and doing
/// it inline on the async body stream steals the worker thread from every other request on the
/// node for the length of a layer push. Buffers to `MIN_PART` (5 MiB) before handing a batch to
/// `spawn_blocking`, so many small body chunks do not round-trip through a blocking task each —
/// the hasher itself moves into and out of the task, since `sha2`'s state is not `Sync`.
struct BlockingHasher {
    hasher: Option<Hasher>,
    buf: Vec<u8>,
}

impl BlockingHasher {
    fn new(hasher: Hasher) -> Self {
        Self { hasher: Some(hasher), buf: Vec::new() }
    }

    async fn update(&mut self, chunk: &[u8]) -> crate::Result<()> {
        self.buf.extend_from_slice(chunk);
        if self.buf.len() as u64 >= MIN_PART {
            self.flush().await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> crate::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let part = std::mem::take(&mut self.buf);
        let mut hasher = self.hasher.take().expect("BlockingHasher used after finish");
        let hasher = tokio::task::spawn_blocking(move || {
            hasher.update(&part);
            hasher
        })
        .await
        .map_err(|e| crate::err(e.to_string()))?;
        self.hasher = Some(hasher);
        Ok(())
    }

    async fn finish(mut self) -> crate::Result<Digest> {
        self.flush().await?;
        let hasher = self.hasher.take().expect("BlockingHasher used after finish");
        tokio::task::spawn_blocking(move || hasher.finish()).await.map_err(|e| crate::err(e.to_string()))
    }
}

fn new_uuid() -> String {
    let mut buf = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(&buf)
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

fn upload_unknown() -> Response {
    oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", "no such upload")
}

/// The OCI reply for a refusal on a CHUNK — every chunk path streams without a digest, so
/// `WrongDigest` cannot come back from one and a 500 beats a panic if it ever does. The
/// digest-carrying `PUT` maps it to a 400 itself; that is the one path this must not serve.
fn refused(e: Refused) -> Response {
    match e {
        Refused::TooLarge => oci_err(StatusCode::PAYLOAD_TOO_LARGE, "SIZE_INVALID", "layer too large"),
        Refused::WrongDigest => crate::oci_internal(crate::err("digest refused on a chunk")),
        Refused::Failed(e) => crate::oci_internal(e),
    }
}

/// How many parts may be in flight before `pour` waits: bounds memory at `(1 + this) * 5 MiB`
/// per request while still overlapping network with hashing. The bound holds for a CHUNKED source
/// — a hyper body, an S3 or filesystem `get` — where each `put` is at most one chunk. A source
/// that yields the whole layer as one `Bytes` (the `mem://` store) spawns `ceil(len / 5 MiB)`
/// part tasks before `wait_for_capacity` is ever consulted, so memory there is O(N); that store
/// is test-only.
const IN_FLIGHT: usize = 4;

/// Streams `src` to `dest` through a multipart upload, hashing as it goes when `expect` names a
/// digest to verify against. Memory is one 5 MiB part plus `IN_FLIGHT` more, never the layer
/// (see `IN_FLIGHT`). The object lands only on `finish`, and every refusal WE raise aborts first,
/// so nothing half-written — or wrongly named — is ever readable under `dest`. Returns the byte
/// count written.
///
// ponytail: `WriteMultipart::finish` consumes `self` and only aborts when `complete()` fails, so a
// part upload that fails inside `finish` leaves the parts live with no handle left to abort them.
// Cleanup for that case is the bucket's incomplete-multipart lifecycle rule (`deploy/README.md`). Upgrade path: drive `MultipartUpload` directly if we ever need to
// guarantee cleanup without one.
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
    let mut hasher = expect.and_then(|d| Hasher::new(&d.algo)).map(BlockingHasher::new);
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
        metrics::counter!("registry_blob_bytes_in_total").increment(chunk.len() as u64);
        if n > blobs::max_layer() {
            let _ = w.abort().await;
            return Err(Refused::TooLarge);
        }
        if let Some(h) = hasher.as_mut() {
            if let Err(e) = h.update(&chunk).await {
                let _ = w.abort().await;
                return Err(Refused::Failed(e));
            }
        }
        if let Err(e) = w.wait_for_capacity(IN_FLIGHT).await {
            let _ = w.abort().await;
            return Err(Refused::Failed(e.into()));
        }
        w.put(chunk);
    }
    if let Some(want) = expect {
        let got = match hasher {
            Some(h) => match h.finish().await {
                Ok(d) => Some(d),
                Err(e) => {
                    let _ = w.abort().await;
                    return Err(Refused::Failed(e));
                }
            },
            None => None,
        };
        if got.as_ref() != Some(want) {
            let _ = w.abort().await;
            return Err(Refused::WrongDigest);
        }
    }
    w.finish().await.map_err(|e| Refused::Failed(e.into()))?;
    Ok(n)
}

/// The session's bytes so far — its size (from the GET's own meta, so no separate HEAD) and a
/// stream. `None` is no session: the staging object IS the session and `open_session` writes an
/// empty one up front, so a `NotFound` here means it was cancelled or swept — not a fresh
/// two-request push. Resuming at offset 0 in that case would silently resurrect a session the
/// client already gave up on.
pub(super) async fn staged(
    os: &Arc<dyn ObjectStore>,
    path: &OsPath,
) -> crate::Result<Option<(u64, BoxStream<'static, crate::Result<Bytes>>)>> {
    match os.get(path).await {
        Ok(r) => {
            let size = r.meta.size;
            Ok(Some((size, r.into_stream().map_err(crate::Error::from).boxed())))
        }
        Err(slatedb::object_store::Error::NotFound { .. }) => Ok(None),
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
    let cr = cr.trim_start_matches("bytes ");
    // RFC 7233 allows a `/total` suffix (`bytes 0-9/10`); the OCI spec omits it but clients send
    // it. Left in, it made `end` unparseable, which read as "no end declared" and silently
    // switched the length check off for exactly the chunks that declared one.
    let cr = cr.split_once('/').map_or(cr, |(range, _)| range);
    let mut parts = cr.split('-');
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

/// How many bytes the session holds, or `None` when there is no session. Goes through `session`
/// rather than reading the staging object's size, because on the fast path the bytes are in parts
/// the store will not assemble until completion — the staging object is still the empty marker
/// `open_session` wrote, and its size is not the answer.
async fn received(app: &App, owner: &str, name: &str, uuid: &str) -> crate::Result<Option<u64>> {
    Ok(session(app, owner, name, uuid).await?.map(|(n, _)| n))
}

/// `POST /v2/{o}/{n}/blobs/uploads/` with no `digest` — opens a session the client completes with
/// a PUT or PATCHes chunks into.
pub async fn open_session(app: &App, owner: &str, name: &str) -> Response {
    let uuid = new_uuid();
    // The image must exist (even manifest-less) so a completed upload has somewhere to belong.
    if let Err(e) = app.store.touch_image(owner, name).await {
        return crate::oci_internal(e);
    }
    // An EMPTY staging object, written now: the object is the session, so a session with no
    // bytes yet must still be something `received` can find and the sweep can age out.
    if let Err(e) = app.store.os.put(&staging(owner, name, &uuid), PutPayload::default()).await {
        return crate::oci_internal(e.into());
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
        return upload_unknown();
    }
    // Two PATCHes to the same session racing would both read the same `have`, both append to the
    // staging object from that offset, and last-writer-wins clobbers the other's bytes (the digest
    // check at PUT time catches it eventually, but as a confusing failure far from the cause).
    // Serialize the whole read-have -> append -> write sequence per session.
    let lock = app.store.keyed_lock(&format!("upload/{owner}/{name}/{uuid}"));
    let _guard = lock.lock().await;
    let path = staging(&owner, &name, &uuid);
    let (have, sc) = match session(&app, &owner, &name, &uuid).await {
        Ok(Some(s)) => s,
        Ok(None) => return upload_unknown(),
        Err(e) => return crate::oci_internal(e),
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
    // Which path this chunk takes. A session on multipart stays on it whatever the chunk size —
    // `put_parts` grows the tail when a chunk cannot fill a part, and the tail is capped at
    // `MIN_PART` by construction, so memory stays bounded however the client chunks. The
    // multipart starts on the FIRST chunk regardless of its size: gating it on a part-sized first
    // chunk sent every client chunking under 5 MiB down the O(N·K) fallback for the session's
    // whole life. A session with bytes already appended and no sidecar (a pre-sidecar build's, or
    // a `file://` store's) stays on the fallback: its first part would have to be all of them.
    let fast = match &app.store.mp {
        Some(mp) if sc.is_some() || have == 0 => Some(mp.clone()),
        _ => None,
    };
    let len = if let Some(mp) = fast {
        match patch_part(&app, &mp, &owner, &name, &uuid, sc, body).await {
            Ok(len) => len,
            Err(r) => return r,
        }
    } else {
        // Fallback: re-stream the whole session ahead of the new chunk, as this always did. Only
        // reached with no sidecar — a store with no `MultipartStore` (`file://`).
        let src = match staged(&app.store.os, &path).await {
            Ok(Some((_, s))) => s,
            Ok(None) => {
                return upload_unknown()
            }
            Err(e) => return crate::oci_internal(e),
        };
        match pour(&app.store.os, &path, None, src.chain(body_stream(body))).await {
            Ok(len) => len,
            Err(e) => return refused(e),
        }
    };
    // A chunked body with a Content-Range that lied: the session has advanced by what really
    // arrived, and the 400 tells the client so. Its next GET/PATCH sees the true `Range` — that
    // is the resume protocol working, not a corrupted session. `checked_sub` because a session
    // swept mid-request would make `len` smaller than the `have` read before it: that is the same
    // "no session" answer, not an underflow.
    match len.checked_sub(have) {
        Some(arrived) if declared.is_some_and(|d| d != arrived) => return length_mismatch(),
        None => return upload_unknown(),
        _ => {}
    }
    accepted(&owner, &name, &uuid, len)
}

/// The tail the session is holding, then the request body: the bytes this chunk contributes, in
/// order. Prepending the tail is what lets a sub-part-sized remainder ride along into a full-size
/// part instead of forcing an undersized one S3 would reject.
fn tail_then(tail: Bytes, body: Body) -> BoxStream<'static, crate::Result<Bytes>> {
    futures::stream::once(futures::future::ready(Ok(tail))).chain(body_stream(body)).boxed()
}

/// The fast path: this chunk's bytes go up ONCE, as parts of the session's multipart upload.
/// Returns the session's new length.
///
/// The sidecar write is the commit point, and it happens last: a refusal (or a crash) leaves parts
/// uploaded but unreferenced, so the session simply still stands at its previous offset and the
/// client resumes there. Unreferenced parts are reaped by the bucket's incomplete-multipart
/// lifecycle rule — the same ceiling `pour` already names.
async fn patch_part(
    app: &App,
    mp: &Arc<dyn slatedb::object_store::multipart::MultipartStore>,
    owner: &str,
    name: &str,
    uuid: &str,
    sc: Option<Sidecar>,
    body: Body,
) -> Result<u64, Response> {
    let path = staging(owner, name, uuid);
    let fresh = sc.is_none();
    let (mut meta, tail) = match sc {
        Some(s) => (s.meta, s.tail),
        None => {
            let id = mp
                .create_multipart(&path)
                .await
                .map_err(|e| crate::oci_internal(e.into()))?;
            (Meta { id, parts: Vec::new(), len: 0 }, Bytes::new())
        }
    };
    let room = blobs::max_layer().saturating_sub(meta.len);
    let put = put_parts(mp, &path, &meta.id, meta.parts.len(), tail_then(tail, body), false, room)
        .await;
    let (ids, parted, tail) = match put {
        Ok(v) => v,
        Err(e) => {
            // Nothing referenced these parts yet; a multipart WE just opened has no session behind
            // it either, so abort it rather than leave it for the lifecycle rule.
            if fresh {
                let _ = mp.abort_multipart(&path, &meta.id).await;
            }
            return Err(refused(e));
        }
    };
    meta.parts.extend(ids);
    meta.len += parted;
    let sc = Sidecar { meta, tail };
    let payload = sc.encode().map_err(crate::oci_internal)?;
    app.store
        .os
        .put(&sidecar_path(owner, name, uuid), payload)
        .await
        .map_err(|e| crate::oci_internal(e.into()))?;
    Ok(sc.received())
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
        return upload_unknown();
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
        Ok(None) => upload_unknown(),
        Err(e) => crate::oci_internal(e),
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
        return upload_unknown();
    }
    // Same lock `patch`/`complete` hold: a DELETE landing between a concurrent PATCH's read of
    // the staging object and its multipart `finish` would be undone by that finish, resurrecting
    // the session the client just cancelled.
    let lock = app.store.keyed_lock(&format!("upload/{owner}/{name}/{uuid}"));
    let _guard = lock.lock().await;
    // Best effort: a sidecar we cannot read still gets deleted, it just leaves its multipart to the
    // lifecycle rule. Cancelling must not fail on it.
    let sc = session(&app, &owner, &name, &uuid).await.ok().flatten().and_then(|(_, sc)| sc);
    discard(&app, &owner, &name, &uuid, sc.as_ref()).await;
    StatusCode::NO_CONTENT.into_response()
}

/// Staging object FIRST: it is what `session` tests for, so a crash between the two deletes leaves
/// an orphan sidecar that answers 404 (and the sweep reaps), never a session that looks alive.
/// The multipart upload is aborted if one was open — a `sc` we cannot read is left to the bucket's
/// incomplete-multipart lifecycle rule rather than blocking the cancel.
async fn discard(app: &App, owner: &str, name: &str, uuid: &str, sc: Option<&Sidecar>) {
    let _ = app.store.os.delete(&staging(owner, name, uuid)).await;
    if let (Some(mp), Some(sc)) = (&app.store.mp, sc) {
        let _ = mp.abort_multipart(&staging(owner, name, uuid), &sc.meta.id).await;
    }
    let _ = app.store.os.delete(&sidecar_path(owner, name, uuid)).await;
}

/// `PUT /v2/{o}/{n}/blobs/uploads/{uuid}?digest=` — completes a session. A body here is the last
/// chunk, which is how the two-request push (no PATCH ever sent) arrives.
///
// ponytail: completion still reads the assembled blob ONCE to hash it — sha2 has no serializable
// state to carry across requests, and holding the hasher in node memory would lose the session
// when the image moves nodes. That is O(N) per push, not the O(N*K) the PATCH path used to be, and
// it is the floor for a registry that verifies what it stores. On the fast path the verified bytes
// then reach `blobs/` by `copy`, a server-side CopyObject that S3 caps at 5 GiB — a layer above
// that fails the copy and the client retries. Upgrade paths: multipart copy for the >5 GiB case,
// or per-part digests if a client ever offers them.
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
        return upload_unknown();
    }
    let Some(d) = Digest::parse(digest) else {
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "malformed digest");
    };
    // Same session lock `patch` takes (identical key), held across the same read-have -> read
    // staging -> write sequence: a PATCH racing this PUT would otherwise interleave with the
    // append below, surfacing as a DIGEST_INVALID far from the real cause.
    let lock = app.store.keyed_lock(&format!("upload/{owner}/{name}/{uuid}"));
    let _guard = lock.lock().await;
    let (have, sc) = match session(app, owner, name, uuid).await {
        Ok(Some(s)) => s,
        Ok(None) => return upload_unknown(),
        Err(e) => return crate::oci_internal(e),
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
    // Hashed with the CLAIMED algorithm (`d.algo`), not assumed sha256, so a sha512 push is
    // checked as sha512. A mismatch aborts the upload before anything lands under the digest,
    // and the session stays open: a client that mis-stated the digest may retry the PUT.
    let len = match sc {
        Some(sc) => match complete_parts(app, owner, name, uuid, &d, sc, body).await {
            Ok(len) => len,
            Err(r) => return r,
        },
        None => {
            let src = match staged(&app.store.os, &staging(owner, name, uuid)).await {
                Ok(Some((_, s))) => s,
                Ok(None) => {
                    return upload_unknown()
                }
                Err(e) => return crate::oci_internal(e),
            };
            match pour(&app.store.os, &blob_path(owner, &d), Some(&d), src.chain(body_stream(body)))
                .await
            {
                Ok(len) => len,
                Err(Refused::TooLarge) => {
                    return oci_err(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "SIZE_INVALID",
                        "layer too large",
                    )
                }
                Err(Refused::WrongDigest) => {
                    return oci_err(
                        StatusCode::BAD_REQUEST,
                        "DIGEST_INVALID",
                        "content does not match digest",
                    )
                }
                Err(Refused::Failed(e)) => return crate::oci_internal(e),
            }
        }
    };
    // The blob has landed under a digest that matched — content-addressed, so a lying
    // Content-Range on a chunked body costs the client a 400 and a retry, never a wrong object.
    // `len < have` means the session was swept between the `received` above and the read: that is
    // the "no session" answer, not an underflow. The blob itself has already landed, which is
    // harmless — it is content-addressed, so it is either the bytes its digest promises or it is
    // nothing, and the GC sweep reclaims it if no manifest ever references it.
    match len.checked_sub(have) {
        Some(arrived) if declared.is_some_and(|d| d != arrived) => return length_mismatch(),
        None => return upload_unknown(),
        _ => {}
    }
    if let Err(e) = super::store::hold_blob(&app.store, owner, name, &d).await {
        return crate::oci_internal(e);
    }
    // Both branches have already disposed of anything multipart, so there is nothing left to abort.
    discard(app, owner, name, uuid, None).await;
    blobs::created(owner, name, &d)
}

/// Completion on the fast path: last part, `CompleteMultipartUpload`, verify, publish.
///
/// The assembled object lands on the STAGING key, never straight on `blobs/{owner}/…`: the digest
/// is not known until the bytes are read back, and a blob path that turned out to hold something
/// else would have to be deleted — which only a client DELETE and the GC sweep may ever do, and
/// which would clobber a concurrent honest push of the same digest. So it is verified where it is
/// harmless and then `copy`d, server-side, into place.
async fn complete_parts(
    app: &App,
    owner: &str,
    name: &str,
    uuid: &str,
    d: &Digest,
    sc: Sidecar,
    body: Body,
) -> Result<u64, Response> {
    use slatedb::object_store::multipart::PartId;
    let path = staging(owner, name, uuid);
    let Some(mp) = app.store.mp.clone() else {
        // A sidecar can only exist because this node had a `MultipartStore` when it was written,
        // and `mp` is fixed at process start — so this is a misconfiguration, not a client error.
        return Err(crate::oci_internal(crate::err("upload session needs a multipart store")));
    };
    let mut meta = sc.meta;
    let room = blobs::max_layer().saturating_sub(meta.len);
    let (ids, parted, _) =
        put_parts(&mp, &path, &meta.id, meta.parts.len(), tail_then(sc.tail, body), true, room)
            .await
            .map_err(refused)?;
    meta.parts.extend(ids);
    meta.len += parted;
    if meta.parts.is_empty() {
        // Nothing was ever uploaded — a session whose every chunk was a lie about its length.
        // There is no valid `CompleteMultipartUpload` for zero parts, so drop the multipart and let
        // the ordinary verified-write path answer, which it does with a 400 for any real digest.
        let _ = mp.abort_multipart(&path, &meta.id).await;
        let _ = app.store.os.delete(&sidecar_path(owner, name, uuid)).await;
        return Err(oci_err(
            StatusCode::BAD_REQUEST,
            "DIGEST_INVALID",
            "content does not match digest",
        ));
    }
    let parts = meta.parts.iter().map(|c| PartId { content_id: c.clone() }).collect();
    mp.complete_multipart(&path, &meta.id, parts)
        .await
        .map_err(|e| crate::oci_internal(e.into()))?;
    // The multipart is spent, and the staging object now holds the whole blob. Dropping the sidecar
    // turns the session back into an ordinary staged one at the same length — which is exactly what
    // a client retrying after the digest check below fails should find.
    let _ = app.store.os.delete(&sidecar_path(owner, name, uuid)).await;

    let (size, mut src) = match staged(&app.store.os, &path).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            return Err(upload_unknown())
        }
        Err(e) => return Err(crate::oci_internal(e)),
    };
    if size > blobs::max_layer() {
        return Err(oci_err(
            StatusCode::PAYLOAD_TOO_LARGE,
            "SIZE_INVALID",
            "layer too large",
        ));
    }
    let mut h = BlockingHasher::new(
        Hasher::new(&d.algo)
            .ok_or_else(|| oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "malformed digest"))?,
    );
    while let Some(chunk) = src.next().await {
        h.update(&chunk.map_err(crate::oci_internal)?).await.map_err(crate::oci_internal)?;
    }
    if h.finish().await.map_err(crate::oci_internal)? != *d {
        return Err(oci_err(
            StatusCode::BAD_REQUEST,
            "DIGEST_INVALID",
            "content does not match digest",
        ));
    }
    app.store
        .os
        .copy(&path, &blob_path(owner, d))
        .await
        .map_err(|e| crate::oci_internal(e.into()))?;
    Ok(size)
}

/// As an extension trait rather than an inherent `impl Store` — see `registry::store::ImageExt`'s
/// doc comment for why: `Store` lives in the `storage` crate now, and Rust's orphan rule forbids
/// an inherent impl on a foreign type from this crate.
#[allow(async_fn_in_trait)]
pub trait UploadsExt {
    async fn sweep_stale_uploads(&self, owner: &str, grace: std::time::Duration) -> crate::Result<usize>;
}

impl UploadsExt for Store {
    /// Delete this owner's abandoned upload sessions under `uploads/{owner}/`. Object-store reads
    /// and deletes ONLY: this runs in the GC worker, which must never open an image database (the
    /// single-opener invariant). Keep-biased like `gc::sweep_owner`: an entry this can't read is
    /// skipped, never deleted on uncertainty, and one bad entry does not abort the rest.
    ///
    /// A session is judged by its LAST activity, not the staging object's age: on the fast path
    /// the staging object is written empty at open and never touched again, every chunk landing
    /// in the `{uuid}.parts` sidecar instead — so a push that outlives `grace` (a large layer on
    /// a slow link) was being swept mid-flight, 404ing the client back to zero. The newer of the
    /// two objects' timestamps is when the client last spoke; both are kept while that is fresh.
    /// A sidecar being deleted has its multipart upload aborted first, so its parts do not sit in
    /// the bucket until a lifecycle rule (which `deploy/README.md` asks for as belt-and-braces).
    ///
    // ponytail: `upload/{uuid}` rows written by the pre-row-less build are orphaned — a few bytes
    // each in an image's DB, and nothing deletes them. Upgrade path: a one-off `delete_image_rows`
    // -style prefix purge over the owner's images, if the bytes ever matter.
    async fn sweep_stale_uploads(&self, owner: &str, grace: std::time::Duration) -> crate::Result<usize> {
        let prefix = OsPath::from(format!("uploads/{owner}"));
        let cutoff = chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::now() - grace);
        let mut listing = self.os.list(Some(&prefix));
        let mut objects = Vec::new();
        let mut last_activity = std::collections::HashMap::<String, chrono::DateTime<chrono::Utc>>::new();
        while let Some(m) = listing.next().await {
            let Ok(m) = m else { continue }; // keep-biased: an entry this can't read is skipped
            let session = m.location.as_ref().trim_end_matches(".parts").to_string();
            let seen = last_activity.entry(session).or_insert(m.last_modified);
            *seen = (*seen).max(m.last_modified);
            objects.push(m);
        }
        let mut n = 0usize;
        // Listing order is lexical, so a session's staging object comes before its sidecar and is
        // deleted first — the same order `discard` uses, for the same crash-safety reason.
        for m in objects {
            let session = m.location.as_ref().trim_end_matches(".parts");
            if last_activity.get(session).is_some_and(|t| *t > cutoff) {
                continue;
            }
            if m.location.as_ref().ends_with(".parts") {
                // Best effort, as in `cancel`: a sidecar we cannot read is still deleted and its
                // multipart left to the lifecycle rule.
                if let Some(mp) = &self.mp {
                    if let Ok(sc) = self.os.get(&m.location).await {
                        if let Ok(sc) = sc.bytes().await.map_err(crate::Error::from).and_then(Sidecar::decode) {
                            let _ = mp.abort_multipart(&OsPath::from(session), &sc.meta.id).await;
                        }
                    }
                }
            }
            if self.os.delete(&m.location).await.is_ok() {
                n += 1;
            }
        }
        Ok(n)
    }
}

#[cfg(test)]
mod declared_chunk_tests {
    use super::declared_chunk;
    use axum::http::HeaderMap;

    fn with(cr: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("content-range", cr.parse().unwrap());
        h
    }

    /// `bytes 0-9/10` declares ten bytes just as `bytes 0-9` does — the suffix used to turn the
    /// declared length into `None`.
    #[test]
    fn a_total_suffix_still_declares_the_length() {
        assert_eq!(declared_chunk(&with("bytes 0-9/10"), "o", "n", "u", 0).ok(), Some(Some(10)));
        assert_eq!(declared_chunk(&with("0-9/*"), "o", "n", "u", 0).ok(), Some(Some(10)));
        assert_eq!(declared_chunk(&with("bytes 0-9"), "o", "n", "u", 0).ok(), Some(Some(10)));
        assert!(declared_chunk(&with("bytes 5-9/10"), "o", "n", "u", 0).is_err(), "start must match");
    }
}
