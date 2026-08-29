//! Manifests and the tag map.
//!
//! Manifest BYTES are stored verbatim and returned verbatim. The digest is over those exact bytes,
//! so re-serializing a parsed manifest — even to identical-looking JSON — changes the digest and
//! breaks every client that verifies one. Nothing here parses a manifest except to read `subject`
//! for the referrers index.
use super::store::{blob_path, ImageExt};
use super::{
    auth, oci_err,
    store::{manifest_path, Digest},
};
use std::collections::HashSet;
use crate::Trusted;
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

const MEDIA_TYPE_KEY_PREFIX: &str = "image/manifest-type/";

/// The largest manifest accepted. Manifests are lists of digests; anything approaching this is not
/// a manifest. `pub` so `routes.rs` can size the manifest route's `DefaultBodyLimit` off the same
/// number — axum's own default (2 MB) is smaller than this and would otherwise 413 a legal push
/// before `put_manifest` ever runs its own check below.
pub const MAX_MANIFEST: usize = 4 * 1024 * 1024;

/// A reference is either a digest or a tag. Tags are the same shape as any other name segment.
enum Reference {
    Digest(Digest),
    Tag(String),
}

fn reference(s: &str) -> Option<Reference> {
    if let Some(d) = Digest::parse(s) {
        return Some(Reference::Digest(d));
    }
    // OCI tag grammar: [a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}
    let ok = s.len() <= 128
        && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
    ok.then(|| Reference::Tag(s.to_string()))
}

pub async fn put_manifest(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, reference_str)): Path<(String, String, String)>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    body: Bytes,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, true).await {
        return r;
    }
    if body.len() > MAX_MANIFEST {
        return oci_err(StatusCode::from_u16(413).unwrap(), "SIZE_INVALID", "manifest too large");
    }
    // Parsed once, to READ — never re-emitted (the digest is over the bytes as sent). Anything
    // that is not a JSON OBJECT is refused here: `gc::referenced` cannot walk it for the blobs it
    // names and would otherwise abort every sweep for this owner, forever, on one bad push. A
    // bare `[]`, `"x"`, `3` or `null` parses but is no more walkable than garbage.
    let Some(mut v) =
        serde_json::from_slice::<serde_json::Value>(&body).ok().filter(serde_json::Value::is_object)
    else {
        return oci_err(StatusCode::BAD_REQUEST, "MANIFEST_INVALID", "manifest is not a JSON object");
    };
    let Some(r) = reference(&reference_str) else {
        return oci_err(StatusCode::BAD_REQUEST, "MANIFEST_INVALID", "malformed reference");
    };
    // Hash with the algorithm the CLIENT chose: a push by sha512 digest must verify against
    // sha512 and be stored under it, or every sha512 GET after a 201 would be a 404. A push by
    // tag has no claimed algorithm and gets the default.
    let d = match &r {
        Reference::Digest(asked) => match Digest::of_algo(&asked.algo, &body) {
            Some(actual) if &actual == asked => actual,
            _ => {
                return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "content does not match digest")
            }
        },
        Reference::Tag(_) => {
            // A by-tag push declares no algorithm, so sha256 is the default — but these exact
            // bytes may already be stored under ANOTHER algorithm (a client that pushed by
            // sha512 digest and now pushes the same manifest by tag). Repointing the tag at a
            // freshly minted sha256 would silently strip the identity the client already uses,
            // so prefer whichever digest the store already knows these bytes by. The sha512 is
            // hashed only when the sha256 object is absent: the common case pays one HEAD.
            let sha256 = Digest::of(&body);
            if app.store.os.head(&manifest_path(&owner, &name, &sha256)).await.is_ok() {
                sha256
            } else {
                match Digest::of_algo("sha512", &body) {
                    Some(sha512)
                        if app.store.os.head(&manifest_path(&owner, &name, &sha512)).await.is_ok() =>
                    {
                        sha512
                    }
                    _ => sha256,
                }
            }
        }
    };
    // Every blob the manifest names must already be here, or the 201 would promise bytes the
    // registry does not hold (the spec's MANIFEST_BLOB_UNKNOWN). An index names MANIFESTS in
    // `manifests[].digest`, so "here" is either store. `subject` is exempt: a referrer may be
    // pushed before the thing it refers to.
    // ponytail: a sweep can still delete an old blob between this head and the put below — the
    // window is one request wide, down from "forever" when the mtime refresh silently failed on
    // S3. If it ever bites, write a `touch/{owner}/{algo}/{hex}` marker here and have
    // `gc::sweep_owner` treat the marker's mtime as the blob's.
    // `v` is pruned before the walk, not after: `subject` may legally point at a manifest that has
    // not been pushed yet, and a FOREIGN/nondistributable layer (a `urls` list, or a
    // foreign/nondistributable mediaType — Windows base images) is by spec fetched from elsewhere
    // and never held here. GC's walk stays unpruned: over-collecting there only keeps blobs alive.
    if let Some(m) = v.as_object_mut() {
        m.remove("subject");
        if let Some(layers) = m.get_mut("layers").and_then(|l| l.as_array_mut()) {
            layers.retain(|l| {
                let elsewhere = l.get("urls").and_then(|u| u.as_array()).is_some_and(|u| !u.is_empty());
                let foreign = l.get("mediaType").and_then(|t| t.as_str()).is_some_and(|t| {
                    t.contains(".foreign.") || t.contains(".nondistributable.")
                });
                !elsewhere && !foreign
            });
        }
    }
    let mut named = HashSet::new();
    super::gc::collect(&v, &mut named);
    // The walk grabs EVERY value keyed `digest` anywhere in the document, annotations included —
    // so a string that is not a digest is not a malformed manifest, it is a field this walk has no
    // business reading. Skipped rather than refused: the walk's over-collection is only safe
    // because it is advisory in both directions (GC keeps what it over-collects, and this only
    // decides what to probe for), and refusing here turned that leniency into a rejected push.
    let digests: Vec<Digest> = named.iter().filter_map(|s| Digest::parse(s)).collect();
    // Concurrent, not serial: a 40-layer manifest was up to 80 sequential HEADs before the
    // write. Each probe is independent; blob path first because that is where layers live —
    // the manifest path is only hit for an index's entries.
    // ponytail: a sweep can still delete an old blob between this head and the put below —
    // GC is keep-biased and this window is unchanged from the serial version, so it's not new risk.
    let present = futures::future::join_all(digests.iter().map(|bd| async {
        app.store.os.head(&blob_path(&owner, bd)).await.is_ok()
            || app.store.os.head(&manifest_path(&owner, &name, bd)).await.is_ok()
    }))
    .await;
    if present.iter().any(|ok| !ok) {
        return oci_err(StatusCode::NOT_FOUND, "MANIFEST_BLOB_UNKNOWN", "manifest references a blob this registry does not hold");
    }
    let media = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/vnd.oci.image.manifest.v1+json")
        .to_string();
    // The media type travels with the manifest: a GET must answer the same Content-Type the push
    // declared, and the bytes themselves are not re-parsed to recover it.
    let db = match app.store.image_db(&owner, &name).await {
        Ok(d) => d,
        Err(e) => return crate::oci_internal(e),
    };
    // Read BEFORE the put: it is the one row written for every manifest and no other, so it is
    // what says whether this digest is new to the image — which is what the manifest counter needs
    // and what used to cost a full prefix LIST. A read error reads as "already there", which only
    // ever under-counts; the GC reconcile is what corrects drift either way.
    let existed = db
        .get(format!("{MEDIA_TYPE_KEY_PREFIX}{d}").into_bytes())
        .await
        .map(|v| v.is_some())
        .unwrap_or(true);
    if let Err(e) = app.store.os.put(&manifest_path(&owner, &name, &d), PutPayload::from(body.clone())).await {
        return crate::oci_internal(e.into());
    }
    if let Err(e) = db
        .put(format!("{MEDIA_TYPE_KEY_PREFIX}{d}").into_bytes(), media.into_bytes())
        .await
    {
        return crate::oci_internal(e.into());
    }
    // Blob rows before the tag: a stranger resolving the tag must never find a layer this image
    // does not yet admit holding.
    if let Err(e) = super::store::note_blobs(&db, &digests, &d.to_string()).await {
        return crate::oci_internal(e);
    }
    // A re-push of the same digest may declare a new Content-Type; the cached answer would keep
    // serving the old one otherwise.
    app.store.manifests().remove(&format!("{owner}/{name}/{d}"));
    let subject = match super::referrers::index(&app, &owner, &name, &d, &body).await {
        Ok(s) => s,
        Err(e) => return crate::oci_internal(e),
    };
    if let Reference::Tag(t) = &r {
        if let Err(e) = app.store.put_tag(&owner, &name, t, &d).await {
            return crate::oci_internal(e);
        }
    } else {
        // A push BY DIGEST may still name tags, as `?tag=` query parameters (the spec's tag
        // param, possibly repeated). Each valid one points at this manifest; invalid ones are
        // refused rather than skipped, because a client that asked for a tag and did not get
        // it has been lied to by a 201.
        for (k, v) in form_urlencoded::parse(raw_query.as_deref().unwrap_or("").as_bytes()) {
            if k != "tag" {
                continue;
            }
            if reference(&v).is_none_or(|r| matches!(r, Reference::Digest(_))) {
                return oci_err(StatusCode::BAD_REQUEST, "TAG_INVALID", "malformed tag parameter");
            }
            if let Err(e) = app.store.put_tag(&owner, &name, &v, &d).await {
                return crate::oci_internal(e);
            }
        }
        if let Err(e) = app.store.touch_image(&owner, &name).await {
            return crate::oci_internal(e);
        }
    }
    // Same log-and-continue rule as the marker below, and for the same reason: these counters ARE
    // the marker's inputs, and the GC reconcile rewrites a marker that has drifted.
    if let Err(e) = app.store.note_manifest_put(&owner, &name, existed).await {
        tracing::warn!(owner = %owner, name = %name, error = %e, "manifest count img");
    }
    // Marker is a view, never the source of truth: log-and-continue rather than fail a push that
    // already landed the manifest and tag(s).
    if let Err(e) = app.store.refresh_image_marker(&owner, &name).await {
        tracing::warn!(owner = %owner, name = %name, error = %e, "index refresh img");
    }
    let mut resp = (
        StatusCode::CREATED,
        [
            (header::LOCATION, format!("/v2/{owner}/{name}/manifests/{d}")),
            (header::HeaderName::from_static("docker-content-digest"), d.to_string()),
        ],
    )
        .into_response();
    // Spec: a manifest with a `subject` MUST get `OCI-Subject` on the 201, so a client can tell
    // without a GET that the push was indexed as a referrer.
    if let Some(subject) = subject {
        resp.headers_mut().insert(
            header::HeaderName::from_static("oci-subject"),
            subject.to_string().parse().unwrap(),
        );
    }
    resp
}

pub async fn get_manifest(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path(p): Path<(String, String, String)>,
) -> Response {
    manifest_response(app, trusted, headers, p, true).await
}

pub async fn head_manifest(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path(p): Path<(String, String, String)>,
) -> Response {
    manifest_response(app, trusted, headers, p, false).await
}

async fn manifest_response(
    app: Arc<App>,
    trusted: Trusted,
    headers: HeaderMap,
    (owner, name, reference_str): (String, String, String),
    with_body: bool,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, false).await {
        return r;
    }
    let Some(r) = reference(&reference_str) else {
        return oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such manifest");
    };
    let d = match r {
        Reference::Digest(d) => d,
        Reference::Tag(t) => match app.store.tag(&owner, &name, &t).await {
            Ok(Some(d)) => {
                // The pull counter. GET by tag only — a HEAD is docker probing, and a GET by
                // digest is docker re-reading what the tag already resolved to; counting either
                // would inflate. A map increment only — no lock, no write — so a hundred
                // concurrent pulls of one tag do not queue behind each other here.
                if with_body {
                    app.store.bump_pulls(&owner, &name, &t);
                }
                d
            }
            Ok(None) => return oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such tag"),
            Err(e) => return crate::oci_internal(e),
        },
    };
    let cache_key = format!("{owner}/{name}/{d}");
    if let Some((bytes, media)) = app.store.manifests().get(&cache_key).cloned() {
        let hdrs = [
            (header::CONTENT_TYPE, media),
            (header::CONTENT_LENGTH, bytes.len().to_string()),
            (header::HeaderName::from_static("docker-content-digest"), d.to_string()),
        ];
        return if with_body { (StatusCode::OK, hdrs, bytes).into_response() } else { (StatusCode::OK, hdrs).into_response() };
    }
    let bytes = match app.store.os.get(&manifest_path(&owner, &name, &d)).await {
        Ok(r) => match r.bytes().await {
            Ok(b) => b,
            Err(e) => return crate::oci_internal(e.into()),
        },
        Err(slatedb::object_store::Error::NotFound { .. }) => {
            return oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such manifest")
        }
        Err(e) => return crate::oci_internal(e.into()),
    };
    let media = match app.store.image_db(&owner, &name).await {
        Ok(db) => db
            .get(format!("{MEDIA_TYPE_KEY_PREFIX}{d}").into_bytes())
            .await
            .ok()
            .flatten()
            .map(|v| String::from_utf8_lossy(&v).to_string())
            .unwrap_or_else(|| "application/vnd.oci.image.manifest.v1+json".into()),
        Err(e) => return crate::oci_internal(e),
    };
    {
        let mut c = app.store.manifests();
        // ponytail: clear-on-full at 256 entries (≤ 256 × 4 MiB worst case, ~a few MiB real) —
        // the same sweep-don't-evict shape as auth_cache. A real LRU if hit rate ever matters.
        if c.len() >= 256 {
            c.clear();
        }
        c.insert(cache_key, (bytes.clone(), media.clone()));
    }
    let hdrs = [
        (header::CONTENT_TYPE, media),
        (header::CONTENT_LENGTH, bytes.len().to_string()),
        (header::HeaderName::from_static("docker-content-digest"), d.to_string()),
    ];
    if with_body {
        (StatusCode::OK, hdrs, bytes).into_response()
    } else {
        (StatusCode::OK, hdrs).into_response()
    }
}

/// By tag: unlink the tag. By digest: remove the manifest AND every tag that pointed at it —
/// leaving a tag resolving to bytes that are gone would turn every pull of it into a 404 the owner
/// cannot explain.
pub async fn delete_manifest(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name, reference_str)): Path<(String, String, String)>,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, true).await {
        return r;
    }
    let Some(r) = reference(&reference_str) else {
        return oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such manifest");
    };
    // `image_db` creates what it opens; a delete aimed at nothing must not leave a phantom image
    // for the listing (and the worker's reconcile) to find.
    match app.store.image_exists(&owner, &name).await {
        Ok(true) => {}
        Ok(false) => return oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such manifest"),
        Err(e) => return crate::oci_internal(e),
    }
    match r {
        Reference::Tag(t) => match app.store.tag(&owner, &name, &t).await {
            Ok(Some(_)) => match app.store.delete_tag(&owner, &name, &t).await {
                Ok(()) => StatusCode::ACCEPTED.into_response(),
                Err(e) => crate::oci_internal(e),
            },
            Ok(None) => oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such tag"),
            Err(e) => crate::oci_internal(e),
        },
        Reference::Digest(d) => {
            let tags = match app.store.tags_pointing_at(&owner, &name, &d).await {
                Ok(t) => t,
                Err(e) => return crate::oci_internal(e),
            };
            for t in tags {
                if let Err(e) = app.store.delete_tag(&owner, &name, &t).await {
                    return crate::oci_internal(e);
                }
            }
            if let Err(e) = super::referrers::unindex(&app, &owner, &name, &d).await {
                return crate::oci_internal(e);
            }
            // The media-type row lives in the image DB, not the object store, so
            // it survives independently of the manifest object below — delete it
            // here or it's an orphan forever (never swept, never read again).
            match app.store.image_db(&owner, &name).await {
                Ok(db) => {
                    if let Err(e) = db.delete(format!("{MEDIA_TYPE_KEY_PREFIX}{d}").into_bytes()).await {
                        return crate::oci_internal(e.into());
                    }
                    if let Err(e) = super::store::forget_manifest_blobs(&db, &d).await {
                        return crate::oci_internal(e);
                    }
                }
                Err(e) => return crate::oci_internal(e),
            }
            app.store.manifests().remove(&format!("{owner}/{name}/{d}"));
            match app.store.os.delete(&manifest_path(&owner, &name, &d)).await {
                Ok(()) => {
                    if let Err(e) = app.store.note_manifest_deleted(&owner, &name).await {
                        tracing::warn!(owner = %owner, name = %name, error = %e, "manifest count img");
                    }
                    StatusCode::ACCEPTED.into_response()
                }
                Err(slatedb::object_store::Error::NotFound { .. }) => {
                    oci_err(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", "no such manifest")
                }
                Err(e) => crate::oci_internal(e.into()),
            }
        }
    }
}

/// `GET /tags/list?n=&last=` — lexical order, `last` exclusive, `Link` when truncated.
pub async fn tags_list(
    State(app): State<Arc<App>>,
    Extension(trusted): Extension<Trusted>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Err(r) = auth::allow(&app, &trusted, &headers, &owner, &name, false).await {
        return r;
    }
    let all = match app.store.tags(&owner, &name).await {
        Ok(t) => t,
        Err(e) => return crate::oci_internal(e),
    };
    if all.is_empty() && !app.store.image_exists(&owner, &name).await.unwrap_or(false) {
        return oci_err(StatusCode::NOT_FOUND, "NAME_UNKNOWN", "no such image");
    }
    let (page, truncated) = super::paginate(&all, &q);
    let body = serde_json::json!({"name": format!("{owner}/{name}"), "tags": page});
    let mut r = axum::Json(body).into_response();
    if let Some(last) = truncated {
        let n = q.get("n").cloned().unwrap_or_default();
        r.headers_mut().insert(
            header::LINK,
            format!("</v2/{owner}/{name}/tags/list?n={n}&last={last}>; rel=\"next\"")
                .parse()
                .unwrap(),
        );
    }
    r
}
