//! Blob pull and the two single-shot push forms. Chunked upload lives in `uploads.rs`.
use super::{auth, oci_err, store::blob_path, store::ImageExt, Digest};
use crate::Trusted;
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

/// Largest single layer accepted by default. NOT a taste: the multipart fast path verifies the
/// assembled blob on its staging key and then `copy`s it into `blobs/`, and that server-side
/// CopyObject is capped at 5 GiB (see `uploads::complete`) — a bigger default accepts a push,
/// pays the whole O(N) hash, and dies at the last step.
pub const DEFAULT_MAX_LAYER: u64 = 5 * 1024 * 1024 * 1024;

/// Largest single layer accepted, checked against the body's size BEFORE it is stored: an
/// unbounded push must not be able to fill a node's disk. `KLOUDLITE_GIT_MAX_LAYER` overrides it and
/// has exactly one setter — `tests/registry_limits.rs`, which is its own test binary precisely
/// because this is a process-wide `OnceLock`. No Deployment sets it; the default is the ceiling
/// in production and changing it means a code change, which is the intent.
pub fn max_layer() -> u64 {
    static LAYER: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *LAYER.get_or_init(|| {
        std::env::var("KLOUDLITE_GIT_MAX_LAYER").ok().and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MAX_LAYER)
    })
}

/// The live value — `cluster/settings`-overridable, unlike `max_layer()` above, which stays a
/// boot-time `OnceLock` because its one caller (`routes.rs`'s `DefaultBodyLimit` layer) is
/// decorative: axum's body-limit layer does not apply to the raw `Body` extractor these routes
/// take, so the real enforcement is the streaming count in `uploads.rs`, and THAT is what reads
/// this instead. The "one true un-cache" in the central settings set (see Task 4's brief): the
/// admin write path can now lower or raise the layer cap without a restart.
pub fn max_layer_live(app: &App) -> u64 {
    app.central.load().max_layer
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
    let who = match auth::allow(&app, &trusted, &headers, &owner, &name, false).await {
        Ok(who) => who,
        Err(r) => return r,
    };
    let Some(d) = Digest::parse(&digest) else {
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "malformed digest");
    };
    // `allow` only proved the caller may pull THIS image; the bytes live per owner. A stranger
    // gets the blob only if this image holds it — and a 404, not a 403, when it does not, so the
    // URL of a public image is not an existence oracle for its private siblings' layers.
    if who.as_deref() != Some(owner.as_str()) {
        match super::store::image_holds_blob(&app.store, &owner, &name, &d).await {
            Ok(true) => {}
            Ok(false) => return oci_err(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "no such blob"),
            Err(e) => return crate::oci_internal(e),
        }
    }
    let path = blob_path(&owner, &d);
    let hdrs = |size: u64| {
        [
            (header::CONTENT_LENGTH, size.to_string()),
            (header::CONTENT_TYPE, "application/octet-stream".into()),
            (header::HeaderName::from_static("docker-content-digest"), d.to_string()),
        ]
    };
    if !with_body {
        return match app.store.os.head(&path).await {
            Ok(m) => (StatusCode::OK, hdrs(m.size)).into_response(),
            Err(slatedb::object_store::Error::NotFound { .. }) => {
                oci_err(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "no such blob")
            }
            Err(e) => crate::oci_internal(e.into()),
        };
    }
    // One GET, not HEAD-then-GET: the GET's own meta carries the size, and this is the hottest
    // registry path — the HEAD was a pure extra round trip per layer pulled.
    // Stream the layer straight through: buffering the whole object here is an anonymous
    // memory-DoS for public images (a few concurrent pulls of a large layer OOM the node).
    match app.store.os.get(&path).await {
        Ok(r) => {
            let size = r.meta.size;
            // Counted at the start, not per chunk streamed: what the store served is what was
            // paid for, and a client that hangs up early is not the interesting number.
            metrics::counter!("registry_blob_bytes_out_total").increment(size);
            (StatusCode::OK, hdrs(size), axum::body::Body::from_stream(r.into_stream())).into_response()
        }
        Err(slatedb::object_store::Error::NotFound { .. }) => {
            oci_err(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "no such blob")
        }
        Err(e) => crate::oci_internal(e.into()),
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
            if let Err(e) = super::store::hold_blob(&app.store, &owner, &name, &d).await {
                return crate::oci_internal(e);
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
    match super::uploads::pour(&app.store.os, &blob_path(owner, &d), Some(&d), super::uploads::body_stream(body), max_layer_live(app)).await {
        Ok(_) => {}
        Err(super::uploads::Refused::TooLarge) => {
            return oci_err(StatusCode::PAYLOAD_TOO_LARGE, "SIZE_INVALID", "layer too large")
        }
        Err(super::uploads::Refused::WrongDigest) => {
            return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "content does not match digest")
        }
        Err(super::uploads::Refused::Failed(e)) => return crate::oci_internal(e),
    }
    // The image now exists, even with no manifest yet: a push that uploads layers and then fails
    // should leave something the owner can see and clean up. `hold_blob`, never
    // `set_image_visibility` — a push must not flip a public image back to private.
    if let Err(e) = super::store::hold_blob(&app.store, owner, name, &d).await {
        return crate::oci_internal(e);
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
        Ok(()) => {
            // The rows say this image HOLDS these bytes, so they must not outlive them — the
            // mirror of `forget_manifest_blobs` on the manifest path. A row cleanup that fails
            // is logged, never a failed delete: the object is already gone, and a stale row only
            // ever grants a pull the store then answers 404 for.
            match app.store.image_db(&owner, &name).await {
                Ok(db) => {
                    if let Err(e) = super::store::forget_blob_rows(&db, &d).await {
                        tracing::warn!(owner = %owner, name = %name, digest = %d, error = %e, "blob delete: hold rows");
                    }
                }
                Err(e) => {
                    tracing::warn!(owner = %owner, name = %name, digest = %d, error = %e, "blob delete: hold rows");
                }
            }
            StatusCode::ACCEPTED.into_response()
        }
        Err(slatedb::object_store::Error::NotFound { .. }) => {
            oci_err(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", "no such blob")
        }
        Err(e) => crate::oci_internal(e.into()),
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

#[cfg(test)]
mod tests {
    /// The default is not a taste: on the multipart fast path a verified blob reaches `blobs/`
    /// by a server-side CopyObject, which S3 caps at 5 GiB (`uploads::complete`'s ponytail note).
    /// A default above that accepts a layer, uploads it, hashes it, and only then 500s.
    #[test]
    fn the_default_layer_cap_is_the_copy_cap() {
        assert_eq!(super::DEFAULT_MAX_LAYER, 5 * 1024 * 1024 * 1024);
        assert_eq!(super::DEFAULT_MAX_LAYER, 5_368_709_120);
    }
}
